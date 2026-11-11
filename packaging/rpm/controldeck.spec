Name:           controldeck
Version:        0.1.0
Release:        1%{?dist}
Summary:        Lightweight web-based server management control panel
License:        AGPL-3.0-or-later
URL:            https://github.com/banshee-lab/controldeck
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo >= 1.85
BuildRequires:  gcc
BuildRequires:  systemd-rpm-macros

%description
ControlDeck serves a dashboard with live and historical system metrics and
package management. It ships two systemd services: controldeck (web,
unprivileged) and controldeck-agent (root, reachable only by the web service).

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked --offline --workspace

%install
install -Dpm0755 target/release/controldeck %{buildroot}%{_bindir}/controldeck
install -Dpm0755 target/release/controldeck-agent %{buildroot}%{_bindir}/controldeck-agent
install -Dpm0644 packaging/controldeck.service %{buildroot}%{_unitdir}/controldeck.service
install -Dpm0644 packaging/controldeck-agent.service %{buildroot}%{_unitdir}/controldeck-agent.service
install -Dpm0644 packaging/controldeck.env %{buildroot}%{_sysconfdir}/controldeck/controldeck.env
install -Dpm0600 packaging/agent.env %{buildroot}%{_sysconfdir}/controldeck/agent.env

%pre
getent group controldeck >/dev/null || groupadd -r controldeck
getent passwd controldeck >/dev/null || useradd -r -g controldeck -d /var/lib/controldeck -M -s /sbin/nologin -c "ControlDeck web server" controldeck
%service_add_pre controldeck-agent.service controldeck.service

%post
%service_add_post controldeck-agent.service controldeck.service

%preun
%service_del_preun controldeck-agent.service controldeck.service

%postun
%service_del_postun controldeck-agent.service controldeck.service

%files
%license LICENSE
%{_bindir}/controldeck
%{_bindir}/controldeck-agent
%{_unitdir}/controldeck.service
%{_unitdir}/controldeck-agent.service
%dir %{_sysconfdir}/controldeck
%config(noreplace) %{_sysconfdir}/controldeck/controldeck.env
%config(noreplace) %attr(0600,root,root) %{_sysconfdir}/controldeck/agent.env