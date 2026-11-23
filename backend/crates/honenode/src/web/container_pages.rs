use askama::Template;
use axum::{extract::{Query, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Redirect, Response}, Form};
use serde::Deserialize;
use crate::{audit, error::{AppError, AppResult}, models::Principal, portal_catalog::CatalogEntry, service_options::{self,Secret}, state::AppState};
use super::{middleware::{self,Current}, layout::{render,principal_view,PrincipalView,GOLD}};

#[derive(Deserialize)]
pub struct ContainerQuery {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    port: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    plan_code: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerForm {
    csrf:String, name:String, plan_code:String, image:String,
    container_port:String, user:String, #[serde(default)] environment:Secret,
    #[serde(default)] volumes:String, read_only:Option<String>,
}

// Deliberately excludes environment: even invalid secret values must never reach a template.
struct FormValues {
    name:String, plan_code:String, image:String, container_port:String,
    user:String, volumes:String, read_only:bool,
}
impl Default for FormValues {
    fn default() -> Self {
        Self {name:String::new(),plan_code:String::new(),image:String::new(),container_port:"8080".into(),user:"1000".into(),volumes:String::new(),read_only:false}
    }
}
impl ContainerForm {
    fn values(&self) -> FormValues {
        FormValues {name:self.name.clone(),plan_code:self.plan_code.clone(),image:self.image.clone(),
            container_port:self.container_port.clone(),user:self.user.clone(),volumes:self.volumes.clone(),
            read_only:self.read_only.as_deref()==Some("true")}
    }
    fn spec(&self) -> AppResult<crate::containers::ContainerSpec> {
        // Parse here rather than in the extractor so invalid numbers retain the other fields.
        let invalid_number = || AppError::BadRequest("Puerto o UID no válido".into());
        crate::containers::ContainerSpec::from_text(self.image.clone(),
            self.container_port.parse().map_err(|_|invalid_number())?,
            self.user.parse().map_err(|_|invalid_number())?, &self.environment,&self.volumes,
            service_options::checkbox(self.read_only.as_deref())?)
    }
}
#[derive(Template)]
#[template(path="tenant/container_new.html")]
struct NewTemplate {
    title:&'static str,accent_class:&'static str,principal:PrincipalView,slug:String,
    entries:Vec<CatalogEntry>,error:String,form:FormValues,plan_available:bool,
}
fn container_entries(entries:Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    entries.into_iter().filter(|e| e.allowed_kinds.iter().any(|k|k=="container")).collect()
}
async fn view(state:&AppState,principal:&Principal,headers:&HeaderMap,error:String,form:FormValues) -> AppResult<Response> {
    let tenant = super::tenant::catalogue_tenant(state, principal).await?;
    let ctx = audit::Context::from_headers(headers).broker_context(principal)?;
    let entries = container_entries(crate::portal_catalog::load(&state.broker,&tenant.slug,&ctx).await
        .map_err(|_|AppError::Broker("catálogo autorizado no disponible".into()))?);
    let plan_available = entries.iter().any(|e|e.plan_code==form.plan_code);
    Ok(render(&NewTemplate {title:"Nuevo servicio",accent_class:GOLD,principal:principal_view(Some(principal),state.config.theme),slug:tenant.slug,entries,error,form,plan_available})?.into_response())
}
pub async fn new_form(State(state):State<AppState>, headers:HeaderMap,current:Current, Query(query):Query<ContainerQuery>) -> AppResult<Response> {
    let mut form = FormValues::default();
    if let Some(image) = query.image.filter(|value| !value.is_empty()) { form.image = image; }
    if let Some(port) = query.port.filter(|value| !value.is_empty()) { form.container_port = port; }
    if let Some(user) = query.user.filter(|value| !value.is_empty()) { form.user = user; }
    if let Some(name) = query.name.filter(|value| !value.is_empty()) { form.name = name; }
    if let Some(plan_code) = query.plan_code.filter(|value| !value.is_empty()) { form.plan_code = plan_code; }
    view(&state,&current.require()?,&headers,String::new(),form).await
}
pub async fn create(State(state):State<AppState>, headers:HeaderMap,current:Current, form:Result<Form<ContainerForm>,axum::extract::rejection::FormRejection>) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    // Never echo serde form errors: they can contain submitted secret values.
    let Form(form) = form.map_err(|_|AppError::BadRequest("Formulario no válido (valores borrados)".into()))?;
    middleware::check_csrf(&principal,&form.csrf)?;
    let result = async {
        let spec = form.spec()?;
        crate::provisioning::provision_container(&state,&principal,tenant_id,&form.plan_code,&form.name,spec,&audit::Context::from_headers(&headers)).await
    }.await;
    match result {
        Ok(service) => Ok(Redirect::to(&format!("/portal/services/{}",service.id)).into_response()),
        Err(AppError::BadRequest(message)|AppError::Conflict(message)) => {
            let values = form.values();
            drop(form);
            let mut response = view(&state,&principal,&headers,format!(
                "No se pudo reservar: {message}. Se conservan los campos salvo las variables de entorno; vuelve a introducirlas antes de enviar."
            ),values).await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            Ok(response)
        },
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Theme, portal_catalog};

    fn submitted() -> ContainerForm {
        ContainerForm {csrf:"csrf".into(),name:"forge-one".into(),plan_code:"generic".into(),
            image:"approved.example/forgejo:1".into(),container_port:"3000".into(),user:"1000".into(),
            environment:Secret::new("PASSWORD=do-not-render-this"),
            volumes:"data=/var/lib/gitea\nconfig=/etc/gitea".into(),read_only:Some("true".into())}
    }
    fn entries() -> Vec<CatalogEntry> {
        portal_catalog::parse(serde_json::json!([
            {"plan_code":"generic","label":"Generic","memory_high":"512M","memory_max":"1G","cpu_quota_percent":100,"tasks_max":128,"max_services":2,"service_days":30,"allowed_kinds":["container"],"registries":["approved.example"]},
            {"plan_code":"templates","label":"Templates","memory_high":"512M","memory_max":"1G","cpu_quota_percent":100,"tasks_max":128,"max_services":2,"service_days":30,"allowed_kinds":["git"]}
        ])).unwrap()
    }
    fn page(form:&ContainerForm,entries:Vec<CatalogEntry>) -> String {
        NewTemplate {title:"Nuevo servicio",accent_class:GOLD,principal:principal_view(None,Theme::Neutral),
            slug:"t01".into(),plan_available:entries.iter().any(|e|e.plan_code==form.plan_code),
            entries,error:"Variables de entorno borradas".into(),form:form.values()}.render().unwrap()
    }
    #[test]
    fn invalid_form_preserves_fields_but_never_environment() {
        let mut form = submitted();
        form.environment = Secret::new("PASSWORD=do-not-render-this\nPASSWORD=duplicate-secret");
        assert!(form.spec().is_err());
        let html = page(&form,container_entries(entries()));
        for value in ["value=\"forge-one\"", "value=\"generic\" selected", "value=\"approved.example/forgejo:1\"",
            "value=\"3000\"", "value=\"1000\"", "data=/var/lib/gitea\nconfig=/etc/gitea", "value=\"true\" checked"] {
            assert!(html.contains(value),"missing preserved field: {value}");
        }
        assert!(!html.contains("do-not-render-this"));
        assert!(!html.contains("duplicate-secret"));
        let textarea = html.split("id=\"environment\"").nth(1).unwrap().split_once('>').unwrap().1;
        assert!(textarea.starts_with("</textarea>"));
        assert!(!html.contains("value=\"templates\""));
    }
    #[test]
    fn invalid_numbers_and_revoked_plan_are_retained_and_escaped() {
        let mut form = submitted();
        form.container_port = "70000".into();
        form.user = "not-a-uid".into();
        form.plan_code = "revoked".into();
        form.volumes = "</textarea><script>alert(1)</script>".into();
        assert!(form.spec().is_err());
        let html = page(&form,container_entries(entries()));
        assert!(html.contains("value=\"70000\""));
        assert!(html.contains("value=\"not-a-uid\""));
        assert!(html.contains("value=\"revoked\" selected"));
        assert!(html.contains("Plan no disponible"));
        assert!(!html.contains("<script>alert(1)</script>"));
    }
    #[test]
    fn generic_plans_do_not_require_template_images_or_registries() {
        let mut entries = entries();
        entries[0].registries.clear();
        let filtered = container_entries(entries);
        assert_eq!(filtered.len(),1);
        assert_eq!(filtered[0].plan_code,"generic");
        assert!(filtered[0].images.is_empty());
        assert!(submitted().spec().unwrap().validate(&filtered[0].registries).is_err());
    }
}
