/// Trust policy for manifests and artifacts. Chosen once by the caller (for
/// example from `TABLEX_DRIVER_DEV=1`); the library never reads the
/// environment.
///
/// - `Strict`: signed manifest required, only HTTPS sources, Team ID pinned
///   code signatures only.
/// - `Developer`: a `file://` manifest may be unsigned, `file://` artifact
///   URLs are allowed, and ad-hoc signed dylibs (no Team ID) are accepted.
///   Everything fetched over the network is still verified strictly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustPolicy {
    #[default]
    Strict,
    Developer,
}
