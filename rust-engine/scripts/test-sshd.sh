#!/usr/bin/env bash
# Throwaway OpenSSH bastion for the engine-net live tests: container `tablex-test-sshd` on 127.0.0.1:52222.
#
#   rust-engine/scripts/test-sshd.sh start    # idempotent: no-op when it is already running
#   rust-engine/scripts/test-sshd.sh stop     # removes the container and the generated credentials
#   rust-engine/scripts/test-sshd.sh status
#   rust-engine/scripts/test-sshd.sh env      # prints the path of the env file; `source` it, then run the tests
#
# Everything secret (login password, key passphrase, private keys) is generated at `start` into a temp
# directory (default $TMPDIR/tablex-test-sshd, mode 0700) and never lives in the repository. Secrets never
# appear on a command line: the container gets them through a 0600 --env-file, ssh-keygen through SSH_ASKPASS. The server can
# open forwards to the other tablex-test-* databases through host.docker.internal:
#   55432 tablex-test-pg   56379 tablex-test-redis   53306 tablex-test-mysql   53307 tablex-test-mariadb
#
# Live tests:
#   source "$(rust-engine/scripts/test-sshd.sh env)"
#   cargo test --manifest-path rust-engine/Cargo.toml -p engine-net --test live -- --ignored --test-threads=1
set -euo pipefail

NAME=tablex-test-sshd
IMAGE=tablex-test-sshd:local
PORT=${TABLEX_TEST_SSHD_PORT:-52222}
USER_NAME=tablex
DIR=${TABLEX_TEST_SSHD_DIR:-${TMPDIR:-/tmp}/tablex-test-sshd}
ENV_FILE=$DIR/env

running() { [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null || true)" = true ]; }
# $1 random bytes as hex (no pipe into `head`: it would trip pipefail with SIGPIPE).
random() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }

build_image() {
  docker image inspect "$IMAGE" >/dev/null 2>&1 && return
  docker build -q -t "$IMAGE" - <<'DOCKERFILE'
FROM alpine:3
RUN apk add --no-cache openssh-server openssh-server-pam linux-pam \
 && adduser -D -s /bin/sh tablex \
 && install -d -m 700 -o tablex -g tablex /home/tablex/.ssh \
 && printf '%s\n' 'Port 22' 'HostKey /etc/ssh/ssh_host_ed25519_key' 'HostKey /etc/ssh/ssh_host_rsa_key' \
      'HostKey /etc/ssh/ssh_host_ecdsa_key' 'PasswordAuthentication yes' 'KbdInteractiveAuthentication yes' \
      'UsePAM yes' 'PubkeyAuthentication yes' 'PermitRootLogin no' 'AllowTcpForwarding yes' \
      'GatewayPorts no' 'PerSourcePenalties no' 'LogLevel VERBOSE' > /etc/ssh/sshd_config
ENTRYPOINT ["/bin/sh", "-c", "ssh-keygen -A >/dev/null && echo \"tablex:$SSH_PASSWORD\" | chpasswd 2>/dev/null && printf '%s' \"$AUTHORIZED_KEYS_B64\" | base64 -d > /home/tablex/.ssh/authorized_keys && chown tablex:tablex /home/tablex/.ssh/authorized_keys && chmod 600 /home/tablex/.ssh/authorized_keys && exec $(command -v sshd.pam || command -v sshd) -D -e"]
DOCKERFILE
}

generate_credentials() {
  rm -rf "$DIR"
  install -d -m 700 "$DIR"
  ssh-keygen -q -t ed25519 -N '' -C tablex-test -f "$DIR/id_ed25519"
  # The passphrase reaches ssh-keygen through an askpass helper reading the environment, not through argv.
  printf '#!/bin/sh\nprintf "%%s\\n" "$TABLEX_KEY_PASSPHRASE"\n' > "$DIR/askpass.sh"
  chmod 700 "$DIR/askpass.sh"
  SSH_ASKPASS="$DIR/askpass.sh" SSH_ASKPASS_REQUIRE=force TABLEX_KEY_PASSPHRASE="$PASSPHRASE" \
    ssh-keygen -q -t ed25519 -C tablex-test -f "$DIR/id_ed25519_enc" </dev/null
  rm -f "$DIR/askpass.sh"
  ssh-keygen -q -t ecdsa -b 256 -N '' -C tablex-test -f "$DIR/id_ecdsa"
  ssh-keygen -q -t rsa -b 3072 -N '' -C tablex-test -f "$DIR/id_rsa"
  ssh-keygen -q -t rsa -b 3072 -m PEM -N '' -C tablex-test -f "$DIR/id_rsa_pem"
  cat "$DIR"/id_ed25519.pub "$DIR"/id_ed25519_enc.pub "$DIR"/id_ecdsa.pub "$DIR"/id_rsa.pub "$DIR"/id_rsa_pem.pub \
    > "$DIR/authorized_keys"
}

start() {
  if running && [ -f "$ENV_FILE" ]; then
    echo "$NAME already running on 127.0.0.1:$PORT (env: $ENV_FILE)"
    return
  fi
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  build_image
  PASSWORD=$(random 12)
  PASSPHRASE=$(random 12)
  generate_credentials
  (
    umask 077
    printf 'SSH_PASSWORD=%s\nAUTHORIZED_KEYS_B64=%s\n' "$PASSWORD" "$(base64 < "$DIR/authorized_keys" | tr -d '\n')" \
      > "$DIR/container.env"
  )
  docker run -d --name "$NAME" \
    -p "127.0.0.1:$PORT:22" \
    --add-host host.docker.internal:host-gateway \
    --env-file "$DIR/container.env" \
    "$IMAGE" >/dev/null
  rm -f "$DIR/container.env"
  for _ in $(seq 1 60); do
    ssh-keyscan -T 1 -p "$PORT" 127.0.0.1 >/dev/null 2>&1 && break
    sleep 0.5
  done
  ssh-keyscan -T 2 -p "$PORT" 127.0.0.1 >/dev/null 2>&1 || { echo "$NAME did not come up" >&2; docker logs "$NAME" >&2; exit 1; }
  docker exec "$NAME" cat /etc/ssh/ssh_host_ed25519_key.pub > "$DIR/host_ed25519.pub"
  (
    umask 077
    cat > "$ENV_FILE" <<ENV
export TABLEX_TEST_SSHD_HOST=127.0.0.1
export TABLEX_TEST_SSHD_PORT=$PORT
export TABLEX_TEST_SSHD_USER=$USER_NAME
export TABLEX_TEST_SSHD_PASSWORD=$PASSWORD
export TABLEX_TEST_SSHD_PASSPHRASE=$PASSPHRASE
export TABLEX_TEST_SSHD_KEY_ED25519=$DIR/id_ed25519
export TABLEX_TEST_SSHD_KEY_ED25519_ENC=$DIR/id_ed25519_enc
export TABLEX_TEST_SSHD_KEY_ECDSA=$DIR/id_ecdsa
export TABLEX_TEST_SSHD_KEY_RSA=$DIR/id_rsa
export TABLEX_TEST_SSHD_KEY_RSA_PEM=$DIR/id_rsa_pem
export TABLEX_TEST_SSHD_HOST_PUB=$DIR/host_ed25519.pub
export TABLEX_TEST_SSHD_CONTAINER=$NAME
export TABLEX_TEST_SSHD_PG=host.docker.internal:55432
export TABLEX_TEST_SSHD_REDIS=host.docker.internal:56379
ENV
  )
  echo "$NAME ready on 127.0.0.1:$PORT (env: $ENV_FILE)"
}

case "${1:-}" in
  start) start ;;
  stop)
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    rm -rf "$DIR"
    echo "$NAME removed"
    ;;
  status) running && echo "$NAME running on 127.0.0.1:$PORT" || { echo "$NAME not running"; exit 1; } ;;
  env) [ -f "$ENV_FILE" ] && echo "$ENV_FILE" || { echo "run '$0 start' first" >&2; exit 1; } ;;
  *) echo "usage: $0 start|stop|status|env" >&2; exit 2 ;;
esac
