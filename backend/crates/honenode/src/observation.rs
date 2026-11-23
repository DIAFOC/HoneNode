use crate::broker::{ActiveState, CallContext, Observation, Request};
use crate::error::{AppError, AppResult};
use crate::models::{Service, ServiceState};
use crate::registry;
use crate::state::AppState;

// Callers hold the mutation lock across host commands, readback and persistence so an
// older status cannot overwrite a newer lifecycle action in this single-writer process.
pub async fn refresh(
    state: &AppState,
    service: &Service,
    slug: &str,
    context: Option<&CallContext>,
) -> AppResult<Observation> {
    let report = state
        .broker
        .service_status(slug, &service.unit_name, context)
        .await?;
    registry::record_observation(&state.db, service, &report).await?;
    Ok(report)
}

pub async fn execute(
    state: &AppState,
    service: &Service,
    slug: &str,
    request: &Request,
    target: ServiceState,
    context: Option<&CallContext>,
) -> AppResult<()> {
    let context = match context {
        Some(context) => CallContext::clone(context),
        None => CallContext::system("honenode:lifecycle")?,
    };
    if let Err(error) = state.broker.call_ok_with_context(request, &context).await {
        registry::clear_service_observation(&state.db, service).await?;
        return Err(error);
    }
    // A start acknowledgement is not evidence of a running unit. Only record_observation
    // can promote this pending intent, on an explicit ActiveState::Active report.
    let intent = if target == ServiceState::Running {
        ServiceState::Pending
    } else {
        target
    };
    registry::set_service_state(&state.db, service.id, intent, None).await?;
    registry::clear_service_observation(&state.db, service).await?;
    if target != ServiceState::Deleted {
        let report = refresh(state, service, slug, Some(&context)).await?;
        if target == ServiceState::Running
            && matches!(
                report.active_state(),
                ActiveState::Failed | ActiveState::Maintenance
            )
        {
            registry::set_service_state(&state.db, service.id, ServiceState::Failed, None).await?;
            return Err(AppError::Broker(format!(
                "service reports {} after activation",
                report.active_state().as_str()
            )));
        }
    }
    // A deleted unit cannot supply service.status; leave observation unknown, not "inactive".
    Ok(())
}

pub async fn reconcile(state: &AppState) -> AppResult<()> {
    for view in registry::all_services(&state.db).await? {
        // Prefer interactive mutations. No host writes or automatic starts/retries here.
        let Ok(_guard) = state.mutations.try_lock() else {
            return Ok(());
        };
        let Some(service) = registry::service_by_id(&state.db, view.id).await? else {
            continue;
        };
        if service.deleted_at.is_some() || service.state == ServiceState::Deleted {
            continue;
        }
        match refresh(state, &service, &view.tenant_slug, None).await {
            Ok(report) => {
                if matches!(
                    service.state,
                    ServiceState::Suspended | ServiceState::Stopped
                ) && matches!(
                    report.active_state(),
                    ActiveState::Active | ActiveState::Reloading
                ) {
                    tracing::warn!(service_id = service.id, unit = %service.unit_name,
                        observed = report.active_state().as_str(), "service is active despite stop/suspension intent; operator review required");
                }
            }
            Err(error) => {
                registry::clear_service_observation(&state.db, &service).await?;
                tracing::warn!(service_id = service.id, unit = %service.unit_name,
                    error = %error, "service observation unavailable; state is unknown");
            }
        }
    }
    Ok(())
}
