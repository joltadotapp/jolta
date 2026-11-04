#!/bin/sh
# Regenerates the S/MIME fixtures beside this script: a throwaway CA and
# signer certificate for signer@example.com, made fresh each run and deleted
# afterwards. Only the messages are kept. Uses macOS's own openssl (LibreSSL).
set -eu

cd "$(dirname "$0")"
OPENSSL=/usr/bin/openssl
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/signer.ext" <<'EOF'
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = emailProtection
subjectAltName = email:signer@example.com
EOF

$OPENSSL req -x509 -newkey rsa:2048 -nodes -days 3650 -sha256 -subj "/CN=Jolta Test CA" \
    -keyout "$WORK/ca.key" -out "$WORK/ca.pem" 2>/dev/null
$OPENSSL req -newkey rsa:2048 -nodes -sha256 -subj "/CN=Test Signer/emailAddress=signer@example.com" \
    -keyout "$WORK/signer.key" -out "$WORK/signer.csr" 2>/dev/null
$OPENSSL x509 -req -in "$WORK/signer.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" -CAcreateserial \
    -days 3650 -sha256 -extfile "$WORK/signer.ext" -out "$WORK/signer.pem" 2>/dev/null

cat > "$WORK/inner.eml" <<'EOF'
Content-Type: multipart/mixed; boundary="inner"

--inner
Content-Type: text/plain; charset=utf-8

The quarterly numbers are in the attached notes.

--inner
Content-Type: text/plain; name="notes.txt"
Content-Disposition: attachment; filename="notes.txt"

Revenue up, costs flat.
--inner--
EOF

headers() {
    printf 'From: %s\nTo: reader@example.com\nSubject: %s\nDate: Tue, 03 Nov 2026 10:00:00 +0100\nMessage-ID: <%s@example.com>\n' "$1" "$2" "$3"
}

sign() {
    $OPENSSL smime -sign -md sha256 -signer "$WORK/signer.pem" -inkey "$WORK/signer.key" -in "$WORK/inner.eml" "$@"
}

{ headers "Test Signer <signer@example.com>" "Clear-signed numbers" clear; sign; } > clear-signed.eml
{ headers "Test Signer <other@example.com>" "Clear-signed from another address" other; sign; } > clear-signed-other-from.eml
sed 's/Revenue up, costs flat./Revenue up, costs flat!/' clear-signed.eml > clear-signed-altered.eml

{ headers "Test Signer <signer@example.com>" "Opaque-signed numbers" opaque; sign -nodetach; } > opaque-signed.eml

{ headers "Test Signer <signer@example.com>" "Encrypted numbers" encrypted
  $OPENSSL smime -encrypt -aes256 -in "$WORK/inner.eml" "$WORK/signer.pem"; } > encrypted.eml

cat > pgp-signed.eml <<'EOF'
From: Test Signer <signer@example.com>
To: reader@example.com
Subject: PGP-signed numbers
Date: Tue, 03 Nov 2026 10:00:00 +0100
Message-ID: <pgp@example.com>
MIME-Version: 1.0
Content-Type: multipart/signed; micalg=pgp-sha256; protocol="application/pgp-signature"; boundary="pgp"

--pgp
Content-Type: text/plain; charset=utf-8

The quarterly numbers, signed with PGP.
--pgp
Content-Type: application/pgp-signature; name="signature.asc"
Content-Disposition: attachment; filename="signature.asc"

-----BEGIN PGP SIGNATURE-----

iQEzBAEBCAAdFiEEAAAAAAAAAAAAAAAAAAAAAAAAAAAFAmkAAAAACgkQAAAAAAAA
=AAAA
-----END PGP SIGNATURE-----
--pgp--
EOF

echo "Fixtures written to $(pwd)"
