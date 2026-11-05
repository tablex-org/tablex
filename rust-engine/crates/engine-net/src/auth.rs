//! SSH user authentication: password, private key file, ssh-agent, keyboard-interactive.
//!
//! Every method starts with a `none` probe. It costs one round trip and tells us which methods the server
//! accepts, so a secret is only sent through a method the server actually offers.

use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse, Prompt};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{decode_secret_key, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{MethodKind, MethodSet};
use zeroize::Zeroizing;

use crate::error::{AuthError, TunnelError};
use crate::secret::Secret;
use crate::session::{Client, KillSwitch};
use crate::spec::{
    Auth, CancelToken, Hop, KeyboardInteractiveRequest, PromptItem, Prompts, TunnelSpec,
};

/// Largest private key file we are willing to read (real keys are a few KiB).
const MAX_KEY_FILE_BYTES: u64 = 1 << 20;
/// Keyboard-interactive rounds before we assume a misbehaving server.
const MAX_KI_ROUNDS: usize = 8;

type Session = Handle<Client>;
type BoxedAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

struct Ctx<'a> {
    /// Timeouts and algorithm policy, shared by every hop.
    spec: &'a TunnelSpec,
    /// The server being authenticated to: host, user and credential.
    hop: &'a Hop<'a>,
    prompts: &'a Arc<dyn Prompts>,
    cancel: &'a CancelToken,
    /// Set when the socket reports EOF / an error (faster than russh closing its handle).
    down: &'a tokio::sync::watch::Sender<bool>,
    kill: &'a KillSwitch,
    /// The stored password may be sent once per open: one step only, never a later round or factor.
    password_spent: AtomicBool,
}

impl Ctx<'_> {
    /// Bounds one network round trip by the auth timeout.
    async fn step<T, E: std::fmt::Display>(
        &self,
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, TunnelError> {
        let limit = self.spec.timeouts.auth;
        match tokio::time::timeout(limit, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(self.connect_error(e.to_string())),
            Err(_) => Err(TunnelError::Timeout {
                operation: "SSH authentication",
                seconds: limit.as_secs(),
            }),
        }
    }

    /// The SSH session is gone (russh closed its handle or the socket ended).
    fn transport_lost(&self, session: &Session) -> bool {
        session.is_closed() || *self.down.borrow() || self.kill.peer_closed()
    }

    fn connect_error(&self, reason: String) -> TunnelError {
        TunnelError::Connect {
            host: self.hop.host.to_owned(),
            port: self.hop.port,
            reason,
        }
    }

    /// The configured password, the first time it is asked for and never again.
    fn take_stored_password(&self) -> Option<&Secret> {
        match self.hop.auth {
            Auth::Password(password) if !self.password_spent.swap(true, Ordering::AcqRel) => {
                Some(password)
            }
            _ => None,
        }
    }

    /// Turns an authentication answer into an outcome. An empty method list on a closed session means the
    /// connection died mid-authentication, which is not a rejected credential.
    fn judge(
        &self,
        session: &Session,
        result: AuthResult,
        method: &'static str,
    ) -> Result<Outcome, TunnelError> {
        match result {
            AuthResult::Success => Ok(Outcome::Done),
            AuthResult::Failure {
                remaining_methods,
                partial_success,
            } if partial_success
                && remaining_methods.contains(&MethodKind::KeyboardInteractive) =>
            {
                Ok(Outcome::ContinueWithKeyboardInteractive)
            }
            AuthResult::Failure {
                remaining_methods, ..
            } => {
                if remaining_methods.is_empty() && self.transport_lost(session) {
                    Err(self.connect_error(
                        "the server closed the connection during authentication".into(),
                    ))
                } else {
                    Err(AuthError::Rejected {
                        method,
                        remaining: method_names(&remaining_methods),
                    }
                    .into())
                }
            }
        }
    }
}

enum Outcome {
    Done,
    /// The server accepted this factor and wants keyboard-interactive next.
    ContinueWithKeyboardInteractive,
}

fn method_names(methods: &MethodSet) -> String {
    let names: Vec<&str> = methods.iter().map(<&'static str>::from).collect();
    if names.is_empty() {
        "nothing".to_owned()
    } else {
        names.join(", ")
    }
}

fn offers(methods: &MethodSet, wanted: MethodKind) -> bool {
    // Some servers answer the probe with an empty list; do not refuse to try in that case.
    methods.is_empty() || methods.contains(&wanted)
}

pub(crate) async fn authenticate(
    session: &mut Session,
    spec: &TunnelSpec,
    hop: &Hop<'_>,
    prompts: &Arc<dyn Prompts>,
    cancel: &CancelToken,
    down: &tokio::sync::watch::Sender<bool>,
    kill: &KillSwitch,
) -> Result<(), TunnelError> {
    let ctx = Ctx {
        spec,
        hop,
        prompts,
        cancel,
        down,
        kill,
        password_spent: AtomicBool::new(false),
    };
    let probe = ctx
        .step(session.authenticate_none(hop.user.to_owned()))
        .await?;
    let methods = match probe {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods,
    };

    match hop.auth {
        Auth::Password(_) => password_auth(&ctx, session, &methods).await,
        Auth::KeyFile { path, passphrase } => {
            key_file_auth(&ctx, session, &methods, path, passphrase.as_ref()).await
        }
        Auth::Agent { socket } => agent_auth(&ctx, session, &methods, socket.as_deref()).await,
        Auth::KeyboardInteractive => {
            require_method(&methods, MethodKind::KeyboardInteractive)?;
            keyboard_interactive(&ctx, session, false).await
        }
    }
}

fn require_method(methods: &MethodSet, wanted: MethodKind) -> Result<(), AuthError> {
    offers(methods, wanted)
        .then_some(())
        .ok_or(AuthError::NoUsableMethod)
}

async fn password_auth(
    ctx: &Ctx<'_>,
    session: &mut Session,
    methods: &MethodSet,
) -> Result<(), TunnelError> {
    // An empty method list (some servers answer the probe that way) does not stop us from trying.
    if offers(methods, MethodKind::Password) {
        let password = ctx
            .take_stored_password()
            .ok_or(AuthError::NoUsableMethod)?;
        let result = ctx
            .step(
                session
                    .authenticate_password(ctx.hop.user.to_owned(), password.expose().to_owned()),
            )
            .await?;
        return match ctx.judge(session, result, "password")? {
            Outcome::Done => Ok(()),
            // A second factor after the password goes to the callback; the password is spent.
            Outcome::ContinueWithKeyboardInteractive => {
                keyboard_interactive(ctx, session, false).await
            }
        };
    }
    // PAM-only servers expose the password through keyboard-interactive.
    require_method(methods, MethodKind::KeyboardInteractive)?;
    keyboard_interactive(ctx, session, true).await
}

async fn key_file_auth(
    ctx: &Ctx<'_>,
    session: &mut Session,
    methods: &MethodSet,
    path: &Path,
    passphrase: Option<&Secret>,
) -> Result<(), TunnelError> {
    require_method(methods, MethodKind::PublicKey)?;
    let (owned_path, owned_passphrase) = (path.to_owned(), passphrase.cloned());
    // Decrypting (bcrypt-pbkdf / scrypt) is deliberately slow: keep it off the runtime workers.
    let key = tokio::task::spawn_blocking(move || {
        load_private_key(&owned_path, owned_passphrase.as_ref())
    })
    .await
    .map_err(|_| TunnelError::Closed)??;
    let hash = rsa_hash(ctx, session, key.algorithm().is_rsa()).await;
    let result = ctx
        .step(session.authenticate_publickey(
            ctx.hop.user.to_owned(),
            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
        ))
        .await?;
    match ctx.judge(session, result, "private key")? {
        Outcome::Done => Ok(()),
        Outcome::ContinueWithKeyboardInteractive => keyboard_interactive(ctx, session, false).await,
    }
}

/// RSA signature hash to use. The lookup can wait up to a second for the server's extension info, so it is
/// only done for RSA keys, and it is bounded by the auth timeout. Without server hints SHA-256 is used; the
/// SHA-1 `ssh-rsa` signature only when the spec allows legacy algorithms.
async fn rsa_hash(ctx: &Ctx<'_>, session: &Session, is_rsa: bool) -> Option<HashAlg> {
    if !is_rsa {
        return None;
    }
    let looked_up =
        tokio::time::timeout(ctx.spec.timeouts.auth, session.best_supported_rsa_hash()).await;
    match looked_up {
        Ok(Ok(Some(Some(hash)))) => Some(hash),
        Ok(Ok(Some(None))) if ctx.spec.allow_legacy_algorithms => None,
        _ => (!ctx.spec.allow_legacy_algorithms).then_some(HashAlg::Sha256),
    }
}

fn is_encrypted_error(error: &russh::keys::Error) -> bool {
    matches!(error, russh::keys::Error::KeyIsEncrypted)
}

/// Encrypted containers russh does not report as `KeyIsEncrypted` when no passphrase is given: PKCS#8
/// (`ENCRYPTED PRIVATE KEY`, it fails with a DER error) and PuTTY `.ppk` (an `Encryption:` line other than
/// `none`). OpenSSH-format and legacy `DEK-Info` PEM keys are covered by [`is_encrypted_error`].
fn looks_encrypted(text: &str) -> bool {
    text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        || text.lines().any(|line| {
            line.strip_prefix("Encryption:")
                .is_some_and(|cipher| !cipher.trim().eq_ignore_ascii_case("none"))
        })
}

fn load_private_key(path: &Path, passphrase: Option<&Secret>) -> Result<PrivateKey, AuthError> {
    let unreadable = |reason: String| AuthError::KeyUnreadable {
        path: path.to_owned(),
        reason,
    };
    // Before opening: opening a FIFO (or a device) blocks until someone writes to it.
    let found = std::fs::metadata(path).map_err(|e| unreadable(e.to_string()))?;
    if !found.is_file() {
        return Err(unreadable("not a regular file".into()));
    }
    let file = std::fs::File::open(path).map_err(|e| unreadable(e.to_string()))?;
    // Again on the open file: the checks below must describe what is actually read.
    let metadata = file.metadata().map_err(|e| unreadable(e.to_string()))?;
    if !metadata.is_file() {
        return Err(unreadable("not a regular file".into()));
    }
    // Like OpenSSH: refuse a private key other users can read. Checked on the open file, not the path.
    #[cfg(unix)]
    {
        let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o777;
        if mode & 0o077 != 0 {
            return Err(AuthError::KeyPermissions {
                path: path.to_owned(),
                mode,
            });
        }
    }
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| unreadable(e.to_string()))?;
    if text.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(unreadable(format!(
            "larger than {} MiB",
            MAX_KEY_FILE_BYTES >> 20
        )));
    }

    // First without a passphrase: that separates "needs one" from "malformed" and from "wrong passphrase".
    let error = match decode_secret_key(&text, None) {
        Ok(key) => return Ok(key),
        Err(error) => error,
    };
    if !is_encrypted_error(&error) && !looks_encrypted(&text) {
        return Err(unreadable(error.to_string()));
    }
    let Some(passphrase) = passphrase.filter(|p| !p.is_empty()) else {
        return Err(AuthError::PassphraseRequired {
            path: path.to_owned(),
        });
    };
    decode_secret_key(&text, Some(passphrase.expose())).map_err(|_| AuthError::BadPassphrase {
        path: path.to_owned(),
    })
}

async fn connect_agent(socket: Option<&Path>) -> Result<BoxedAgent, AuthError> {
    #[cfg(unix)]
    {
        let agent = match socket {
            Some(path) => AgentClient::connect_uds(path).await,
            None => AgentClient::connect_env().await,
        };
        agent
            .map(AgentClient::dynamic)
            .map_err(|e| AuthError::AgentUnavailable(e.to_string()))
    }
    #[cfg(windows)]
    {
        const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
        match socket {
            Some(pipe) => AgentClient::connect_named_pipe(pipe)
                .await
                .map(AgentClient::dynamic),
            None => match AgentClient::connect_named_pipe(OPENSSH_PIPE).await {
                Ok(agent) => Ok(agent.dynamic()),
                Err(_) => AgentClient::connect_pageant()
                    .await
                    .map(AgentClient::dynamic),
            },
        }
        .map_err(|e| AuthError::AgentUnavailable(e.to_string()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = socket;
        Err(AuthError::AgentUnavailable(
            "ssh-agent is not supported on this platform".into(),
        ))
    }
}

async fn agent_auth(
    ctx: &Ctx<'_>,
    session: &mut Session,
    methods: &MethodSet,
    socket: Option<&Path>,
) -> Result<(), TunnelError> {
    require_method(methods, MethodKind::PublicKey)?;
    let limit = ctx.spec.timeouts.auth;
    let mut agent = tokio::time::timeout(limit, connect_agent(socket))
        .await
        .map_err(|_| TunnelError::Timeout {
            operation: "ssh-agent connect",
            seconds: limit.as_secs(),
        })??;
    let identities = ctx.step(agent.request_identities()).await?;
    let hash = rsa_hash(ctx, session, identities.iter().any(is_rsa_identity)).await;

    for identity in identities {
        let attempt = match &identity {
            AgentIdentity::PublicKey { key, .. } => {
                ctx.step(session.authenticate_publickey_with(
                    ctx.hop.user.to_owned(),
                    key.clone(),
                    hash,
                    &mut agent,
                ))
                .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                ctx.step(session.authenticate_certificate_with(
                    ctx.hop.user.to_owned(),
                    certificate.clone(),
                    hash,
                    &mut agent,
                ))
                .await
            }
        };
        match attempt {
            Ok(result) => match ctx.judge(session, result, "ssh-agent identity") {
                Ok(Outcome::Done) => return Ok(()),
                Ok(Outcome::ContinueWithKeyboardInteractive) => {
                    return keyboard_interactive(ctx, session, false).await
                }
                Err(TunnelError::Auth(_)) => {}
                Err(other) => return Err(other),
            },
            // The agent failed to sign, or the transport broke. russh leaves its session loop waiting for the
            // signature after a signer error, so the session cannot be reused for another identity: report
            // what happened (a dead transport is a connect error, an agent problem is `AgentUnavailable`).
            Err(TunnelError::Connect { reason, .. }) => {
                return Err(if ctx.transport_lost(session) {
                    ctx.connect_error(reason)
                } else {
                    AuthError::AgentUnavailable(reason).into()
                });
            }
            Err(other) => return Err(other),
        }
    }
    Err(AuthError::NoAgentIdentity.into())
}

fn is_rsa_identity(identity: &AgentIdentity) -> bool {
    match identity {
        AgentIdentity::PublicKey { key, .. } => key.algorithm().is_rsa(),
        AgentIdentity::Certificate { certificate, .. } => certificate.algorithm().is_rsa(),
    }
}

/// `allow_stored_password`: the server offers no `password` method, so the configured password may answer the
/// one password-looking prompt of the *first* round (and only that one). Everything else goes to the
/// [`Prompts`] callback.
async fn keyboard_interactive(
    ctx: &Ctx<'_>,
    session: &mut Session,
    allow_stored_password: bool,
) -> Result<(), TunnelError> {
    let mut response = ctx
        .step(
            session
                .authenticate_keyboard_interactive_start(ctx.hop.user.to_owned(), None::<String>),
        )
        .await?;
    let mut autofilled = false;
    for round in 0..MAX_KI_ROUNDS {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => {
                if remaining_methods.is_empty() && ctx.transport_lost(session) {
                    return Err(ctx.connect_error(
                        "the server closed the connection during authentication".into(),
                    ));
                }
                return Err(AuthError::Rejected {
                    method: if autofilled {
                        "password"
                    } else {
                        "keyboard-interactive answers"
                    },
                    remaining: method_names(&remaining_methods),
                }
                .into());
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                let autofill = allow_stored_password && round == 0;
                let asks_password_again = lone_password_prompt(&prompts);
                let answers = match answer_prompts(
                    ctx,
                    autofill,
                    name,
                    instructions,
                    prompts,
                    &mut autofilled,
                )
                .await
                {
                    // The password was tried and the server asks for it again: that is a wrong password,
                    // not a missing callback. Any other declined prompt (a cancelled one-time code) stays a
                    // declined prompt.
                    Err(TunnelError::Auth(AuthError::PromptDeclined))
                        if autofilled && asks_password_again =>
                    {
                        return Err(AuthError::Rejected {
                            method: "password",
                            remaining: "keyboard-interactive".into(),
                        }
                        .into());
                    }
                    other => other?,
                };
                let answers: Vec<String> = answers.iter().map(|a| a.expose().to_owned()).collect();
                response = ctx
                    .step(session.authenticate_keyboard_interactive_respond(answers))
                    .await?;
            }
        }
    }
    Err(AuthError::Rejected {
        method: "keyboard-interactive answers",
        remaining: "nothing".into(),
    }
    .into())
}

/// Only a lone, non-echoed prompt that asks for *the* account password qualifies for the stored password.
/// One-time codes, tokens and the prompts of a password change are never answered with it.
fn lone_password_prompt(prompts: &[Prompt]) -> bool {
    const NOT_THE_PASSWORD: [&str; 11] = [
        "one-time",
        "one time",
        "onetime",
        "otp",
        "verification",
        "code",
        "token",
        "passcode",
        "new ",
        "retype",
        "again",
    ];
    let [only] = prompts else {
        return false;
    };
    let text = only.prompt.to_ascii_lowercase();
    !only.echo
        && text.contains("password")
        && !NOT_THE_PASSWORD.iter().any(|word| text.contains(word))
}

async fn answer_prompts(
    ctx: &Ctx<'_>,
    autofill: bool,
    name: String,
    instructions: String,
    prompts: Vec<Prompt>,
    autofilled: &mut bool,
) -> Result<Vec<Secret>, TunnelError> {
    let looks_like_password = lone_password_prompt(&prompts);
    if autofill && looks_like_password {
        if let Some(password) = ctx.take_stored_password() {
            *autofilled = true;
            return Ok(vec![password.clone()]);
        }
    }
    if prompts.is_empty() {
        return Ok(Vec::new());
    }
    let request = KeyboardInteractiveRequest {
        host: ctx.hop.host.to_owned(),
        port: ctx.hop.port,
        name,
        instructions,
        prompts: prompts
            .iter()
            .map(|p| PromptItem {
                text: p.prompt.clone(),
                echo: p.echo,
            })
            .collect(),
        cancel: ctx.cancel.clone(),
    };
    ask_callback(ctx.prompts.clone(), request, ctx.spec.timeouts.prompt).await
}

/// The callback may block on a human, so it runs on a blocking worker and is bounded by `limit`. On timeout
/// its [`CancelToken`] is cancelled so it can stop waiting.
async fn ask_callback(
    prompts: Arc<dyn Prompts>,
    request: KeyboardInteractiveRequest,
    limit: Duration,
) -> Result<Vec<Secret>, TunnelError> {
    let expected = request.prompts.len();
    let cancel = request.cancel.clone();
    let task = tokio::task::spawn_blocking(move || prompts.keyboard_interactive(&request));
    match tokio::time::timeout(limit, task).await {
        Err(_) => {
            cancel.cancel();
            Err(TunnelError::Timeout {
                operation: "keyboard-interactive prompt",
                seconds: limit.as_secs(),
            })
        }
        Ok(Err(_)) => Err(TunnelError::Closed),
        Ok(Ok(Some(replies))) if replies.len() == expected => Ok(replies),
        Ok(Ok(_)) => Err(AuthError::PromptDeclined.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names_are_readable() {
        let mut methods = MethodSet::empty();
        assert_eq!(method_names(&methods), "nothing");
        methods.push(MethodKind::PublicKey);
        methods.push(MethodKind::KeyboardInteractive);
        assert_eq!(method_names(&methods), "publickey, keyboard-interactive");
    }

    #[test]
    fn an_empty_probe_answer_does_not_block_trying() {
        let empty = MethodSet::empty();
        assert!(offers(&empty, MethodKind::PublicKey));
        let mut only_password = MethodSet::empty();
        only_password.push(MethodKind::Password);
        assert!(!offers(&only_password, MethodKind::PublicKey));
    }

    #[cfg(unix)]
    fn write_key(dir: &Path, name: &str, contents: &[u8], mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_readable_private_keys_are_refused_naming_path_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_key(dir.path(), "id_open", b"irrelevant", 0o644);
        let err = load_private_key(&path, None).unwrap_err();
        assert!(
            matches!(err, AuthError::KeyPermissions { mode: 0o644, .. }),
            "{err:?}"
        );
        let shown = err.to_string();
        assert!(
            shown.contains("id_open") && shown.contains("0644"),
            "{shown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_file_larger_than_limit_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let big = write_key(
            dir.path(),
            "id_big",
            &vec![b'A'; MAX_KEY_FILE_BYTES as usize + 10],
            0o600,
        );
        let err = load_private_key(&big, None).unwrap_err();
        assert!(
            matches!(&err, AuthError::KeyUnreadable { reason, .. } if reason.contains("larger")),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_malformed_key_is_not_reported_as_a_passphrase_problem() {
        let dir = tempfile::tempdir().unwrap();
        let junk = write_key(dir.path(), "id_junk", b"this is not a key\n", 0o600);
        for passphrase in [None, Some(Secret::new("pass-needle"))] {
            let err = load_private_key(&junk, passphrase.as_ref()).unwrap_err();
            assert!(matches!(err, AuthError::KeyUnreadable { .. }), "{err:?}");
        }
    }

    /// A deterministic throwaway ed25519 key (never used outside these tests).
    #[cfg(unix)]
    fn test_key() -> PrivateKey {
        use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
        PrivateKey::new(
            KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
            "engine-net test",
        )
        .unwrap()
    }

    /// PKCS#8 `ENCRYPTED PRIVATE KEY`: russh reports a DER error, not `KeyIsEncrypted`, without a passphrase.
    #[cfg(unix)]
    #[test]
    fn encrypted_pkcs8_key_decrypts_with_passphrase() {
        let key = test_key();
        let mut pem = Vec::new();
        russh::keys::encode_pkcs8_pem_encrypted(&key, b"pass-needle", 10, &mut pem).unwrap();
        assert!(String::from_utf8_lossy(&pem).contains("BEGIN ENCRYPTED PRIVATE KEY"));
        let dir = tempfile::tempdir().unwrap();
        let path = write_key(dir.path(), "id_pkcs8", &pem, 0o600);

        let loaded = load_private_key(&path, Some(&Secret::new("pass-needle"))).unwrap();
        assert_eq!(loaded.public_key().key_data(), key.public_key().key_data());
        let missing = load_private_key(&path, None).unwrap_err();
        assert!(
            matches!(missing, AuthError::PassphraseRequired { .. }),
            "{missing:?}"
        );
        let wrong = load_private_key(&path, Some(&Secret::new("other-needle"))).unwrap_err();
        assert!(
            matches!(wrong, AuthError::BadPassphrase { .. }),
            "{wrong:?}"
        );
        let shown = format!("{wrong} {wrong:?}");
        assert!(!shown.contains("other-needle"), "{shown}");
    }

    /// An unencrypted PKCS#8 key still loads without a passphrase (and with a needless one).
    #[cfg(unix)]
    #[test]
    fn plain_pkcs8_key_loads_with_or_without_a_passphrase() {
        let key = test_key();
        let mut pem = Vec::new();
        russh::keys::encode_pkcs8_pem(&key, &mut pem).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write_key(dir.path(), "id_plain", &pem, 0o600);
        for passphrase in [None, Some(Secret::new("unused"))] {
            let loaded = load_private_key(&path, passphrase.as_ref()).unwrap();
            assert_eq!(loaded.public_key().key_data(), key.public_key().key_data());
        }
    }

    /// PuTTY `.ppk`: the `Encryption:` header decides; `none` is not an encrypted key.
    #[cfg(unix)]
    #[test]
    fn encrypted_ppk_key_asks_for_its_passphrase() {
        assert!(looks_encrypted(
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: aes256-cbc\n"
        ));
        assert!(!looks_encrypted(
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\n"
        ));
        assert!(!looks_encrypted("-----BEGIN PRIVATE KEY-----\n"));
        let dir = tempfile::tempdir().unwrap();
        // Truncated on purpose: only the classification is under test, not the PPK cipher.
        let ppk = b"PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: aes256-cbc\nComment: test\nPublic-Lines: 0\n";
        let path = write_key(dir.path(), "id.ppk", ppk, 0o600);
        let missing = load_private_key(&path, None).unwrap_err();
        assert!(
            matches!(missing, AuthError::PassphraseRequired { .. }),
            "{missing:?}"
        );
    }

    /// Opening a FIFO blocks until a writer appears: the key path must be refused before that.
    #[cfg(unix)]
    #[test]
    fn fifo_key_path_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("id_fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let (done, finished) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || done.send(load_private_key(&path, None)).ok());
        let outcome = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("loading a FIFO key path must not block");
        let err = outcome.unwrap_err();
        assert!(
            matches!(&err, AuthError::KeyUnreadable { reason, .. } if reason.contains("regular file")),
            "{err:?}"
        );
    }

    #[test]
    fn the_stored_password_only_answers_the_account_password_prompt() {
        let prompt = |text: &str, echo: bool| Prompt {
            prompt: text.into(),
            echo,
        };
        for text in [
            "Password:",
            "Password for tester@bastion: ",
            "(current) UNIX password: ",
        ] {
            assert!(lone_password_prompt(&[prompt(text, false)]), "{text}");
        }
        for text in [
            "One-time password (OATH) for tester:",
            "OTP password:",
            "Verification code:",
            "New password:",
            "Retype new password:",
            "Enter password again:",
            "Duo passcode or password token:",
            "Username:",
        ] {
            assert!(!lone_password_prompt(&[prompt(text, false)]), "{text}");
        }
        assert!(
            !lone_password_prompt(&[prompt("Password:", true)]),
            "echoed prompts never qualify"
        );
        assert!(!lone_password_prompt(&[
            prompt("Password:", false),
            prompt("Password:", false)
        ]));
        assert!(!lone_password_prompt(&[]));
    }

    #[test]
    fn unreadable_key_file_names_the_path_not_the_content() {
        let err = load_private_key(
            Path::new("/nonexistent-dir-for-test/id_ed25519"),
            Some(&Secret::new("pass-needle")),
        )
        .unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(shown.contains("id_ed25519"));
        assert!(!shown.contains("pass-needle"));
    }
}
