%global debug_package %{nil}

%if 0%{?rhel} != 10
%error This experimental prebuilt-artifact spec is gated to RHEL 10 builds only
%endif

Name:           honed
Version:        0.8.8
Release:        0.1.experimental%{?dist}
Summary:        Experimental HoneNode host agent only (application not included)
License:        GPL-3.0-or-later

Source0:        honed
Source1:        honed.service
Source2:        honenode.sysusers.conf
Source3:        honenode.tmpfiles.conf
Source4:        README.honed
Source5:        LICENSE

BuildRequires:  binutils
BuildRequires:  systemd-rpm-macros
Requires(pre):  systemd
Requires(post): systemd
Requires:       coreutils
Requires:       podman
Requires:       shadow-utils
Requires:       systemd
Requires:       util-linux

%description
Experimental packaging for the prebuilt honed host agent. This RPM does not
contain, install, configure, enable, or start the honenode application and is
not a HoneNode product release. No tenant allowlist or active broker policy is
installed. Host integration and SELinux readiness remain operator tasks.

%prep

%build
# The input is prebuilt; verify only that it is an ELF artifact, not source.
/usr/bin/readelf -h %{S:0} >/dev/null

%install
rm -rf %{buildroot}
install -Dpm0755 %{S:0} %{buildroot}%{_bindir}/honed
install -Dpm0644 %{S:1} %{buildroot}%{_unitdir}/honed.service
install -Dpm0644 %{S:2} %{buildroot}%{_sysusersdir}/honenode.conf
install -Dpm0644 %{S:3} %{buildroot}%{_tmpfilesdir}/honenode.conf
install -Dpm0644 %{S:4} %{buildroot}%{_docdir}/honed/README
install -Dpm0644 %{S:5} %{buildroot}%{_licensedir}/honed/LICENSE
install -dpm0750 %{buildroot}%{_sysconfdir}/honenode
install -dpm0750 %{buildroot}%{_sharedstatedir}/honenode
install -dpm0750 %{buildroot}/run/honenode

%pre
%sysusers_create_compat %{S:2}

%post
/usr/bin/systemd-tmpfiles --create %{_tmpfilesdir}/honenode.conf
if [ -d /run/systemd/system ]; then
    /usr/bin/systemctl daemon-reload >/dev/null 2>&1 || :
fi

%postun
if [ "$1" -eq 0 ] && [ -d /run/systemd/system ]; then
    /usr/bin/systemctl daemon-reload >/dev/null 2>&1 || :
fi

%files
%{_bindir}/honed
%{_unitdir}/honed.service
%{_sysusersdir}/honenode.conf
%{_tmpfilesdir}/honenode.conf
%dir %attr(0750,root,root) %{_sysconfdir}/honenode
%dir %attr(0750,root,honenode) %{_sharedstatedir}/honenode
%ghost %dir %attr(0750,root,honenode) /run/honenode
%doc %{_docdir}/honed/README
%license %{_licensedir}/honed/LICENSE

%changelog
* Mon Nov 09 2026 HoneNode packaging prototype <packaging@example.invalid> - 0.1.0-0.1.experimental
- Add an experimental host-agent-only prebuilt-artifact RPM prototype.
