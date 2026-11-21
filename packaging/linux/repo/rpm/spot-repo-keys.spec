%global debug_package %{nil}

# Template: tools/packaging/repo/build_keys_package.py substitutes the markers
# and stages the public keys and the rendered repository file. Public keys only.
Name:           spot-repo-keys
Version:        @VERSION@
Release:        @RELEASE@%{?dist}
Summary:        Spotledger package repository keys and configuration
License:        Apache-2.0
URL:            https://spotledger.example/
BuildArch:      noarch
Source0:        spot.repo
@KEYSOURCES@

%description
The public signing keys of the Spotledger package repositories and the dnf
repository file that checks package and metadata signatures with them.

%prep

%build

%install
rm -rf %{buildroot}
install -d -m 0755 %{buildroot}%{_sysconfdir}/yum.repos.d
install -m 0644 %{SOURCE0} %{buildroot}%{_sysconfdir}/yum.repos.d/spot.repo
install -d -m 0755 %{buildroot}%{_sysconfdir}/pki/rpm-gpg
@KEYINSTALL@

%files
%config(noreplace) %{_sysconfdir}/yum.repos.d/spot.repo
@KEYFILES@

%changelog
@CHANGELOG@
