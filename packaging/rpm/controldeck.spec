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
ControlDeck serves a dashboard with live and historical system metrics.
It is managed through systemd.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked --offline

%install
install -Dpm0755 target/release/controldeck %{buildroot}%{_bindir}/controldeck
install -Dpm0644 packaging/controldeck.service %{buildroot}%{_unitdir}/controldeck.service
install -Dpm0644 packaging/controldeck.env %{buildroot}%{_sysconfdir}/controldeck/controldeck.env

%post
%service_add_post controldeck.service

%preun
%service_del_preun controldeck.service

%postun
%service_del_postun controldeck.service

%files
%license LICENSE
%{_bindir}/controldeck
%{_unitdir}/controldeck.service
%config(noreplace) %{_sysconfdir}/controldeck/controldeck.env