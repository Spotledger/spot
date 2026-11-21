#!/usr/bin/env bash
# Throwaway certificates for the DEVELOPMENT compose file. Needs openssl.
#
#   ./gen-dev-certs.sh DIR
#
# Writes under DIR: operator-ca/ca.pem, host-ca/ca.{pem,key}, tls/*-server.{pem,key}, an operator
# client certificate (operator/dev-operator.{pem,key}) and the health probe's (probe/*). Every key
# is 0600. Nothing it writes is ever committed or baked into an image; production uses an operator
# CA and a host CA from your own PKI, see the project process notes.
set -euo pipefail
out="${1:?usage: gen-dev-certs.sh DIR}"
command -v openssl >/dev/null || { echo "openssl not found" >&2; exit 3; }
umask 077
mkdir -p "$out"/{operator-ca,host-ca,tls,operator,probe}

ca() { # name dir
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 \
        -subj "/CN=$1" -keyout "$2/ca.key" -out "$2/ca.pem" \
        -addext "basicConstraints=critical,CA:TRUE,pathlen:0" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
}
leaf() { # cn ca_dir key_out cert_out usage san
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" -keyout "$3" -out "$3.csr" 2>/dev/null
    printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=%s\n%s' "$5" "${6:+subjectAltName=$6}" >"$3.ext"
    openssl x509 -req -in "$3.csr" -CA "$2/ca.pem" -CAkey "$2/ca.key" -CAcreateserial -days 30 -extfile "$3.ext" -out "$4" 2>/dev/null
    rm -f "$3.csr" "$3.ext"
}
ca "Spotledger dev operator CA" "$out/operator-ca"
ca "Spotledger dev host CA" "$out/host-ca"
leaf "control-plane-operator" "$out/operator-ca" "$out/tls/operator-server.key" "$out/tls/operator-server.pem" serverAuth "DNS:localhost,IP:127.0.0.1"
leaf "control-plane-host" "$out/host-ca" "$out/tls/host-server.key" "$out/tls/host-server.pem" serverAuth "DNS:localhost,IP:127.0.0.1"
leaf "dev-operator" "$out/operator-ca" "$out/operator/dev-operator.key" "$out/operator/dev-operator.pem" clientAuth
leaf "health-probe" "$out/operator-ca" "$out/probe/client.key" "$out/probe/client.pem" clientAuth
cp "$out/operator-ca/ca.pem" "$out/probe/server-ca.pem"
rm -f "$out/operator-ca/ca.key"   # the operator CA key is not needed by the service; keep it out of the mount
chmod 0600 "$out"/*/*.key
echo "dev certificates written to $out (valid 30 days, throwaway)"
