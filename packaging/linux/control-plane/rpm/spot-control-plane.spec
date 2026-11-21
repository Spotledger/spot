%global debug_package %{nil}
%global _hardened_build 1

# Template: packaging/linux/control-plane/build_control_plane.py substitutes the
# version, release, architecture and changelog markers. It packages a prebuilt
# binary; nothing is compiled here. Same structure as packaging/linux/rpm/spot.spec.
Name:           spot-control-plane
Version:        @VERSION@
Release:        @RELEASE@%{?dist}
Summary:        Spotledger control plane
License:        Apache-2.0
URL:            https://spotledger.example/

Source0:        spot-control-plane
Source1:        spot-control-plane.sysusers
Source2:        spot-control-plane.tmpfiles
Source3:        spot-control-plane.service
Source4:        control-plane.toml.example
Source5:        environment
Source6:        LICENSE
Source7:        NOTICE

BuildArch:      @ARCH@
BuildRequires:  systemd-rpm-macros
Requires:       systemd
Requires(pre):  shadow-utils

%description
The Spotledger control plane: tenant registry, licensing, usage, fleet health
and links, served on two mutual-TLS listeners. The package installs the daemon,
a hardened systemd unit and a configuration template. It does not start or
enable the service and ships no key, certificate or password.

%prep

%build

%install
rm -rf %{buildroot}
install -d -m 0755 %{buildroot}%{_bindir}
install -m 0755 %{SOURCE0} %{buildroot}%{_bindir}/spot-control-plane
install -d -m 0755 %{buildroot}%{_unitdir}
install -m 0644 %{SOURCE3} %{buildroot}%{_unitdir}/spot-control-plane.service
install -d -m 0755 %{buildroot}%{_sysusersdir}
install -m 0644 %{SOURCE1} %{buildroot}%{_sysusersdir}/spot-control-plane.conf
install -d -m 0755 %{buildroot}%{_tmpfilesdir}
install -m 0644 %{SOURCE2} %{buildroot}%{_tmpfilesdir}/spot-control-plane.conf
install -d -m 0750 %{buildroot}%{_sysconfdir}/spot-control-plane
install -m 0640 %{SOURCE4} %{buildroot}%{_sysconfdir}/spot-control-plane/control-plane.toml.example
install -m 0600 %{SOURCE5} %{buildroot}%{_sysconfdir}/spot-control-plane/environment
install -d -m 0755 %{buildroot}%{_licensedir}/spot-control-plane
install -m 0644 %{SOURCE6} %{buildroot}%{_licensedir}/spot-control-plane/LICENSE
install -m 0644 %{SOURCE7} %{buildroot}%{_licensedir}/spot-control-plane/NOTICE
install -d -m 0700 %{buildroot}%{_localstatedir}/lib/spot-control-plane

%pre
%sysusers_create_compat %{SOURCE1}

%post
# Nothing starts, enables or configures the service, and no key or secret is written.

%preun
%systemd_preun spot-control-plane.service

%postun
%systemd_postun_with_restart spot-control-plane.service

%files
%{_bindir}/spot-control-plane
%{_unitdir}/spot-control-plane.service
%{_sysusersdir}/spot-control-plane.conf
%{_tmpfilesdir}/spot-control-plane.conf
%dir %attr(0750,root,spot-control-plane) %{_sysconfdir}/spot-control-plane
%config(noreplace) %attr(0640,root,spot-control-plane) %{_sysconfdir}/spot-control-plane/control-plane.toml.example
%config(noreplace) %attr(0600,spot-control-plane,spot-control-plane) %{_sysconfdir}/spot-control-plane/environment
%license %{_licensedir}/spot-control-plane/LICENSE
%license %{_licensedir}/spot-control-plane/NOTICE
%dir %attr(0700,spot-control-plane,spot-control-plane) %{_localstatedir}/lib/spot-control-plane

%changelog
@CHANGELOG@
