#!/usr/bin/env bash
# Throwaway TLS database servers for the driver TLS live tests. Three containers, one throwaway CA:
#
#   tablex-test-pg-tls       postgres:17  127.0.0.1:55433   ssl=on, hostssl only, client-cert auth for `tablex_cert`
#   tablex-test-mysql-tls    mysql:8      127.0.0.1:53316   require_secure_transport=ON, `tablex_x509` REQUIRE X509
#   tablex-test-mariadb-tls  mariadb:11   127.0.0.1:53317   require_secure_transport=ON, `tablex_x509` REQUIRE X509
#   tablex-test-mysql-notls  mysql:8      127.0.0.1:53326   TLS switched off (the existing tablex-test-mysql and
#   tablex-test-mariadb-notls mariadb:11  127.0.0.1:53327   -mariadb auto-generate a certificate, so they always offer TLS):
#                                                           the servers `PREFERRED` must fall back to clear text on
#
#   rust-engine/scripts/test-tls-servers.sh start [pg|mysql|mariadb|mysql-notls|mariadb-notls|all]   # idempotent; default: all
#   rust-engine/scripts/test-tls-servers.sh stop  [pg|mysql|mariadb|mysql-notls|mariadb-notls|all]   # removes the containers (and the certificates with `all`)
#   rust-engine/scripts/test-tls-servers.sh status
#   rust-engine/scripts/test-tls-servers.sh env      # prints the path of the env file; `source` it, then run the tests
#
# The server certificate carries the SANs `localhost` and `db.tablex.test` (and no IP address), so connecting to
# 127.0.0.1 is a host-name mismatch on purpose: the tests then use `hostaddr=127.0.0.1` / `tls-server-name` with the
# host `db.tablex.test`, exactly what an SSH tunnel does. `other-ca.crt` is an unrelated CA ("unknown CA" tests).
#
# Everything secret (CA and server keys, client keys, passwords) is generated at `start` into a temp directory
# (default $TMPDIR/tablex-test-tls, mode 0700) and never lives in the repository. Containers receive it through a
# 0600 --env-file that is deleted right after `docker run`; passwords never appear on a command line (the throwaway
# encrypted client key gets its passphrase through the environment). Only containers named tablex-test-* are touched,
# and the existing tablex-test-pg / -mysql / -mariadb are never modified.
#
# Live tests:
#   source "$(rust-engine/scripts/test-tls-servers.sh env)"
#   TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
#     cargo test --manifest-path rust-engine/Cargo.toml -p dbengine-postgres --features abi-v2 tls -- --ignored --test-threads=1
set -euo pipefail

DIR=${TABLEX_TEST_TLS_DIR:-${TMPDIR:-/tmp}/tablex-test-tls}
ENV_FILE=$DIR/env
PG=tablex-test-pg-tls;      PG_PORT=${TABLEX_TEST_PG_TLS_PORT:-55433}
MYSQL=tablex-test-mysql-tls; MYSQL_PORT=${TABLEX_TEST_MYSQL_TLS_PORT:-53316}
MARIADB=tablex-test-mariadb-tls; MARIADB_PORT=${TABLEX_TEST_MARIADB_TLS_PORT:-53317}
MYSQL_NOTLS=tablex-test-mysql-notls; MYSQL_NOTLS_PORT=${TABLEX_TEST_MYSQL_NOTLS_PORT:-53326}
MARIADB_NOTLS=tablex-test-mariadb-notls; MARIADB_NOTLS_PORT=${TABLEX_TEST_MARIADB_NOTLS_PORT:-53327}
SERVER_NAME=db.tablex.test
DB_USER=tablex
DB_NAME=tablex
CERT_USER=tablex_cert   # PostgreSQL: CN of the client certificate = role name
X509_USER=tablex_x509   # MySQL / MariaDB: REQUIRE X509

running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true)" = true ]; }
# $1 random bytes as hex (no pipe into `head`: it would trip pipefail with SIGPIPE).
random() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
b64() { base64 < "$1" | tr -d '\n'; }

# ---- certificates -----------------------------------------------------------------------------------------------
new_ca() { # $1 basename, $2 common name
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$DIR/$1.key" 2>/dev/null
  openssl req -new -x509 -key "$DIR/$1.key" -sha256 -days 30 -subj "/CN=$2" -out "$DIR/$1.crt" \
    -config /dev/stdin <<'CONF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = ca
[ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
CONF
}

sign() { # $1 csr, $2 out crt, $3 extfile
  openssl x509 -req -in "$1" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" -CAcreateserial -days 30 -sha256 \
    -extfile "$3" -out "$2" 2>/dev/null
}

generate_certificates() {
  rm -rf "$DIR"
  install -d -m 700 "$DIR"
  new_ca ca "TableX Test CA"
  new_ca other-ca "TableX Unrelated CA"
  rm -f "$DIR/other-ca.key"

  # Server: CN db.tablex.test, SAN localhost + db.tablex.test, RSA 2048 (the widest server compatibility).
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$DIR/server.key" 2>/dev/null
  openssl req -new -key "$DIR/server.key" -subj "/CN=$SERVER_NAME" -out "$DIR/server.csr"
  printf '%s\n' 'basicConstraints = CA:FALSE' 'keyUsage = critical,digitalSignature,keyEncipherment' \
    'extendedKeyUsage = serverAuth' "subjectAltName = DNS:localhost,DNS:$SERVER_NAME" > "$DIR/server.ext"
  sign "$DIR/server.csr" "$DIR/server.crt" "$DIR/server.ext"

  # Client certificates, one per private-key encoding drivers must read: PKCS#8, PKCS#1 ("RSA PRIVATE KEY"), SEC1 ("EC PRIVATE KEY").
  printf '%s\n' 'basicConstraints = CA:FALSE' 'keyUsage = critical,digitalSignature' 'extendedKeyUsage = clientAuth' \
    > "$DIR/client.ext"
  client_cert() { # $1 basename (key already at $DIR/$1.key)
    openssl req -new -key "$DIR/$1.key" -subj "/CN=$CERT_USER" -out "$DIR/$1.csr"
    sign "$DIR/$1.csr" "$DIR/$1.crt" "$DIR/client.ext"
  }
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$DIR/client.key" 2>/dev/null # PKCS#8
  client_cert client
  # OpenSSL 3 writes PKCS#8 unless told otherwise; LibreSSL always writes PKCS#1.
  openssl genrsa -traditional 2048 > "$DIR/client-rsa.key" 2>/dev/null || openssl genrsa 2048 > "$DIR/client-rsa.key" 2>/dev/null
  grep -q 'BEGIN RSA PRIVATE KEY' "$DIR/client-rsa.key" || { echo "openssl did not write a PKCS#1 key" >&2; exit 1; }
  client_cert client-rsa
  openssl ecparam -name prime256v1 -genkey -noout -out "$DIR/client-ec.key" 2>/dev/null
  grep -q 'BEGIN EC PRIVATE KEY' "$DIR/client-ec.key" || { echo "openssl did not write a SEC1 key" >&2; exit 1; }
  client_cert client-ec
  # Password-protected PKCS#8 (drivers answer with a typed "not supported").
  TABLEX_KEY_PASSPHRASE=$(random 8) openssl pkcs8 -topk8 -in "$DIR/client.key" -out "$DIR/client-enc.key" \
    -passout env:TABLEX_KEY_PASSPHRASE
  rm -f "$DIR"/*.csr "$DIR"/*.ext "$DIR"/*.srl
  chmod 600 "$DIR"/*.key
}

# ---- containers -------------------------------------------------------------------------------------------------
# Common: the container receives the certificates base64-encoded in its env file and writes them below /tls.
write_tls_files='install -d -m 755 /tls
printf %s "$TLS_CA_B64" | base64 -d > /tls/ca.crt
printf %s "$TLS_CERT_B64" | base64 -d > /tls/server.crt
printf %s "$TLS_KEY_B64" | base64 -d > /tls/server.key
chown -R "$TLS_OWNER" /tls
chmod 644 /tls/ca.crt /tls/server.crt
chmod 600 /tls/server.key'

tls_env() { # appends the shared certificate variables to $1
  printf 'TLS_CA_B64=%s\nTLS_CERT_B64=%s\nTLS_KEY_B64=%s\n' \
    "$(b64 "$DIR/ca.crt")" "$(b64 "$DIR/server.crt")" "$(b64 "$DIR/server.key")" >> "$1"
}

start_pg() {
  running "$PG" && return
  docker rm -f "$PG" >/dev/null 2>&1 || true
  local envf=$DIR/pg.container.env
  (
    umask 077
    printf 'POSTGRES_USER=%s\nPOSTGRES_PASSWORD=%s\nPOSTGRES_DB=%s\nTLS_OWNER=postgres:postgres\nCERT_USER=%s\n' \
      "$DB_USER" "$PG_PASSWORD" "$DB_NAME" "$CERT_USER" > "$envf"
    tls_env "$envf"
  )
  # pg_hba: local trust for the entrypoint's own psql; hostssl only (a plain TCP connection is refused by pg_hba);
  # certificate authentication for CERT_USER, SCRAM for everybody else.
  docker run -d --name "$PG" -p "127.0.0.1:$PG_PORT:5432" --env-file "$envf" --entrypoint /bin/bash postgres:17 -c "
set -e
$write_tls_files
printf '%s\n' 'local all all trust' \"hostssl all \$CERT_USER all cert\" 'hostssl all all all scram-sha-256' > /tls/pg_hba.conf
chown postgres:postgres /tls/pg_hba.conf
printf 'CREATE ROLE %s LOGIN;\nGRANT ALL ON DATABASE %s TO %s;\n' \"\$CERT_USER\" \"\$POSTGRES_DB\" \"\$CERT_USER\" > /docker-entrypoint-initdb.d/10-cert-role.sql
exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tls/server.crt -c ssl_key_file=/tls/server.key \
  -c ssl_ca_file=/tls/ca.crt -c hba_file=/tls/pg_hba.conf -c ssl_min_protocol_version=TLSv1.2
" >/dev/null
  rm -f "$envf"
}

# $1 name, $2 port, $3 image, $4 server binary, $5 env prefix (MYSQL|MARIADB), $6 extra server args, $7 password,
# $8 tls|notls (tls: our certificates + a REQUIRE X509 user; notls: whatever $6 does to switch TLS off)
start_mysql_family() {
  local name=$1 port=$2 image=$3 binary=$4 prefix=$5 extra=$6 password=$7 kind=$8
  running "$name" && return
  docker rm -f "$name" >/dev/null 2>&1 || true
  local envf=$DIR/$name.container.env
  (
    umask 077
    printf '%s_ROOT_PASSWORD=%s\n%s_USER=%s\n%s_PASSWORD=%s\n%s_DATABASE=%s\nTLS_OWNER=mysql:mysql\nX509_USER=%s\nX509_PASSWORD=%s\n' \
      "$prefix" "$ROOT_PASSWORD" "$prefix" "$DB_USER" "$prefix" "$password" "$prefix" "$DB_NAME" \
      "$X509_USER" "$password" > "$envf"
    if [ "$kind" = tls ]; then tls_env "$envf"; fi
  )
  local setup=''
  if [ "$kind" = tls ]; then
    setup="$write_tls_files
printf \"CREATE USER '%s'@'%%' IDENTIFIED BY '%s' REQUIRE X509;\nGRANT ALL ON ${DB_NAME}.* TO '%s'@'%%';\n\" \"\$X509_USER\" \"\$X509_PASSWORD\" \"\$X509_USER\" > /docker-entrypoint-initdb.d/10-x509-user.sql"
    extra="--ssl-ca=/tls/ca.crt --ssl-cert=/tls/server.crt --ssl-key=/tls/server.key $extra"
  fi
  docker run -d --name "$name" -p "127.0.0.1:$port:3306" --env-file "$envf" --entrypoint /bin/bash "$image" -c "
set -e
$setup
exec docker-entrypoint.sh $binary $extra
" >/dev/null
  rm -f "$envf"
}

wait_pg() {
  for _ in $(seq 1 120); do
    docker exec "$PG" pg_isready -q -h 127.0.0.1 -p 5432 >/dev/null 2>&1 && return
    sleep 0.5
  done
  echo "$PG did not come up" >&2; docker logs "$PG" 2>&1 | tail -20 >&2; exit 1
}

wait_greeting() { # $1 container, $2 port: the final server (not the init-time one) greets a TCP client
  for _ in $(seq 1 180); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$2" && read -r -t 2 -n 1 _ <&3) 2>/dev/null; then return; fi
    sleep 0.5
  done
  echo "$1 did not come up" >&2; docker logs "$1" 2>&1 | tail -20 >&2; exit 1
}

# ---- commands ---------------------------------------------------------------------------------------------------
targets() {
  case "${1:-all}" in
    all) echo "pg mysql mariadb mysql-notls mariadb-notls" ;;
    pg|mysql|mariadb|mysql-notls|mariadb-notls) echo "$1" ;;
    *) echo "unknown target '$1'" >&2; exit 2 ;;
  esac
}

start() {
  local want; want=$(targets "${1:-all}")
  if [ ! -f "$ENV_FILE" ]; then
    # Fresh credentials: containers started under an older env file would not match them.
    for c in "$PG" "$MYSQL" "$MARIADB" "$MYSQL_NOTLS" "$MARIADB_NOTLS"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
    generate_certificates
    (
      umask 077
      printf 'PG_PASSWORD=%s\nMYSQL_PASSWORD=%s\nMARIADB_PASSWORD=%s\nROOT_PASSWORD=%s\n' \
        "$(random 12)" "$(random 12)" "$(random 12)" "$(random 12)" > "$DIR/secrets"
    )
  fi
  # shellcheck disable=SC1091
  source "$DIR/secrets"
  for t in $want; do
    case $t in
      pg) start_pg ;;
      mysql) start_mysql_family "$MYSQL" "$MYSQL_PORT" mysql:8 mysqld MYSQL \
               "--require_secure_transport=ON --tls-version=TLSv1.2,TLSv1.3" "$MYSQL_PASSWORD" tls ;;
      mariadb) start_mysql_family "$MARIADB" "$MARIADB_PORT" mariadb:11 mariadbd MARIADB \
               "--require-secure-transport=ON" "$MARIADB_PASSWORD" tls ;;
      # MySQL 8.4 has no --skip-ssl; an empty --tls-version leaves it without TLS.
      mysql-notls) start_mysql_family "$MYSQL_NOTLS" "$MYSQL_NOTLS_PORT" mysql:8 mysqld MYSQL "--tls-version=" "$MYSQL_PASSWORD" notls ;;
      mariadb-notls) start_mysql_family "$MARIADB_NOTLS" "$MARIADB_NOTLS_PORT" mariadb:11 mariadbd MARIADB "--skip-ssl" "$MARIADB_PASSWORD" notls ;;
    esac
  done
  for t in $want; do
    case $t in
      pg) wait_pg ;;
      mysql) wait_greeting "$MYSQL" "$MYSQL_PORT" ;;
      mariadb) wait_greeting "$MARIADB" "$MARIADB_PORT" ;;
      mysql-notls) wait_greeting "$MYSQL_NOTLS" "$MYSQL_NOTLS_PORT" ;;
      mariadb-notls) wait_greeting "$MARIADB_NOTLS" "$MARIADB_NOTLS_PORT" ;;
    esac
  done
  write_env
  echo "TLS test servers ready (env: $ENV_FILE)"
}

write_env() {
  (
    umask 077
    cat > "$ENV_FILE" <<ENV
export TABLEX_TEST_TLS_DIR=$DIR
export TABLEX_TEST_TLS_CA=$DIR/ca.crt
export TABLEX_TEST_TLS_OTHER_CA=$DIR/other-ca.crt
export TABLEX_TEST_TLS_SERVER_NAME=$SERVER_NAME
export TABLEX_TEST_TLS_CLIENT_CERT=$DIR/client.crt
export TABLEX_TEST_TLS_CLIENT_KEY=$DIR/client.key
export TABLEX_TEST_TLS_CLIENT_CERT_RSA=$DIR/client-rsa.crt
export TABLEX_TEST_TLS_CLIENT_KEY_RSA=$DIR/client-rsa.key
export TABLEX_TEST_TLS_CLIENT_CERT_EC=$DIR/client-ec.crt
export TABLEX_TEST_TLS_CLIENT_KEY_EC=$DIR/client-ec.key
export TABLEX_TEST_TLS_CLIENT_KEY_ENCRYPTED=$DIR/client-enc.key
export TABLEX_TEST_PG_TLS_HOST=127.0.0.1
export TABLEX_TEST_PG_TLS_PORT=$PG_PORT
export TABLEX_TEST_PG_TLS_USER=$DB_USER
export TABLEX_TEST_PG_TLS_PASSWORD=$PG_PASSWORD
export TABLEX_TEST_PG_TLS_DB=$DB_NAME
export TABLEX_TEST_PG_TLS_CERT_USER=$CERT_USER
export TABLEX_TEST_MYSQL_TLS_HOST=127.0.0.1
export TABLEX_TEST_MYSQL_TLS_PORT=$MYSQL_PORT
export TABLEX_TEST_MYSQL_TLS_USER=$DB_USER
export TABLEX_TEST_MYSQL_TLS_PASSWORD=$MYSQL_PASSWORD
export TABLEX_TEST_MYSQL_TLS_DB=$DB_NAME
export TABLEX_TEST_MYSQL_TLS_X509_USER=$X509_USER
export TABLEX_TEST_MARIADB_TLS_HOST=127.0.0.1
export TABLEX_TEST_MARIADB_TLS_PORT=$MARIADB_PORT
export TABLEX_TEST_MARIADB_TLS_USER=$DB_USER
export TABLEX_TEST_MARIADB_TLS_PASSWORD=$MARIADB_PASSWORD
export TABLEX_TEST_MARIADB_TLS_DB=$DB_NAME
export TABLEX_TEST_MARIADB_TLS_X509_USER=$X509_USER
export TABLEX_TEST_MYSQL_NOTLS_URI=mysql://$DB_USER:$MYSQL_PASSWORD@127.0.0.1:$MYSQL_NOTLS_PORT/$DB_NAME
export TABLEX_TEST_MARIADB_NOTLS_URI=mariadb://$DB_USER:$MARIADB_PASSWORD@127.0.0.1:$MARIADB_NOTLS_PORT/$DB_NAME
ENV
  )
}

stop() {
  local want; want=$(targets "${1:-all}")
  for t in $want; do
    case $t in
      pg) docker rm -f "$PG" ;; mysql) docker rm -f "$MYSQL" ;; mariadb) docker rm -f "$MARIADB" ;;
      mysql-notls) docker rm -f "$MYSQL_NOTLS" ;; mariadb-notls) docker rm -f "$MARIADB_NOTLS" ;;
    esac >/dev/null 2>&1 || true
  done
  if [ "${1:-all}" = all ]; then rm -rf "$DIR"; echo "TLS test servers removed"; fi
}

case "${1:-}" in
  start) start "${2:-all}" ;;
  stop) stop "${2:-all}" ;;
  status)
    rc=0
    for c in "$PG" "$MYSQL" "$MARIADB" "$MYSQL_NOTLS" "$MARIADB_NOTLS"; do
      if running "$c"; then echo "$c running"; else echo "$c not running"; rc=1; fi
    done
    exit $rc ;;
  env) [ -f "$ENV_FILE" ] && echo "$ENV_FILE" || { echo "run '$0 start' first" >&2; exit 1; } ;;
  *) echo "usage: $0 start|stop [pg|mysql|mariadb|mysql-notls|mariadb-notls|all] | status | env" >&2; exit 2 ;;
esac
