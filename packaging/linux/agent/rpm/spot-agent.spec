%global debug_package %{nil}
%global _hardened_build 1

# Template: packaging/linux/agent/build_agent.py substitutes the version,
# release, architecture and changelog markers. It packages two prebuilt
# binaries and the sudoers drop-in that `spot-agent-priv --print-sudoers`
# generated at package build time; nothing is compiled or hand-written here.
Name:           spot-agent
Version:        @VERSION@
Release:        @RELEASE@%{?dist}
Summary:        Spotledger host agent
License:        Apache-2.0
URL:            https://spotledger.example/

Source0:        spot-agent
Source1:        spot-agent-priv
Source2:        spot-agent.sysusers
Source3:        spot-agent.tmpfiles
Source4:        spot-agent.service
Source5:        spot-agent.sudoers
Source6:        LICENSE
Source7:        NOTICE

BuildArch:      @ARCH@
BuildRequires:  systemd-rpm-macros
Requires:       systemd
Requires:       sudo
Requires(pre):  shadow-utils

%description
The Spotledger host agent runs as an unprivileged account and executes signed
management commands. The few privileged operations go through a small helper
named by an exact entry in /etc/sudoers.d/spot-agent. The package does not
start, enable or enrol the agent: enrolment is an explicit operator action
with a one-time token.

%prep

%build

%install
rm -rf %{buildroot}
install -d -m 0755 %{buildroot}%{_bindir}
install -m 0755 %{SOURCE0} %{buildroot}%{_bindir}/spot-agent
install -d -m 0755 %{buildroot}%{_libexecdir}/spot-agent
install -m 0755 %{SOURCE1} %{buildroot}%{_libexecdir}/spot-agent/spot-agent-priv
install -d -m 0755 %{buildroot}%{_unitdir}
install -m 0644 %{SOURCE4} %{buildroot}%{_unitdir}/spot-agent.service
install -d -m 0755 %{buildroot}%{_sysusersdir}
install -m 0644 %{SOURCE2} %{buildroot}%{_sysusersdir}/spot-agent.conf
install -d -m 0755 %{buildroot}%{_tmpfilesdir}
install -m 0644 %{SOURCE3} %{buildroot}%{_tmpfilesdir}/spot-agent.conf
install -d -m 0750 %{buildroot}%{_sysconfdir}/sudoers.d
install -m 0440 %{SOURCE5} %{buildroot}%{_sysconfdir}/sudoers.d/spot-agent
install -d -m 0755 %{buildroot}%{_licensedir}/spot-agent
install -m 0644 %{SOURCE6} %{buildroot}%{_licensedir}/spot-agent/LICENSE
install -m 0644 %{SOURCE7} %{buildroot}%{_licensedir}/spot-agent/NOTICE
install -d -m 0700 %{buildroot}%{_localstatedir}/lib/spot-agent

%pre
# Downgrade is refused, an upgrade is not: rpm -q still shows the installed
# (old) version here. A refusal exits 1 before any file is replaced; the way
# back is to remove the package (state is kept) and install the older one.
if [ "$1" -ge 2 ]; then
    old=$(rpm -q --qf '%%{EVR}\n' spot-agent 2>/dev/null | head -n 1)
    if [ -n "$old" ]; then
        cmp=$(rpm --eval "%%{lua:print(rpm.vercmp('$old', '%{version}-%{release}'))}")
        if [ "$cmp" = "1" ]; then
            echo "spot-agent: refusing to downgrade from $old to %{version}-%{release}" >&2
            exit 1
        fi
    fi
fi
%sysusers_create_compat %{SOURCE2}

%post
# Nothing starts the agent and nothing enrols it, and no secret is written.
# A running agent is restarted once, after the upgrade, by the postun helper.

%preun
%systemd_preun spot-agent.service

%postun
%systemd_postun_with_restart spot-agent.service

%files
%{_bindir}/spot-agent
%dir %attr(0755,root,root) %{_libexecdir}/spot-agent
%attr(0755,root,root) %{_libexecdir}/spot-agent/spot-agent-priv
%{_unitdir}/spot-agent.service
%{_sysusersdir}/spot-agent.conf
%{_tmpfilesdir}/spot-agent.conf
%attr(0440,root,root) %{_sysconfdir}/sudoers.d/spot-agent
%license %{_licensedir}/spot-agent/LICENSE
%license %{_licensedir}/spot-agent/NOTICE
# The state directory (identity key, trust, audit log) is owned by the package
# for permissions only; erasing the package never deletes its content.
%dir %attr(0700,spot-agent,spot-agent) %{_localstatedir}/lib/spot-agent

%changelog
@CHANGELOG@
