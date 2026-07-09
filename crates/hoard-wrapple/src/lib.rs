//! Hoard-Wrapple — Spotify-Wrapped-style play stats.
//!
//! Pro-only. The actual recap is generated **server-side** (so a patched
//! community client can never fabricate it) and rendered by the desktop Pro UI.
//! Linked into `hoard-server` at build time behind the `pro` feature (optional
//! git dependency); community builds compile a stub in the public repo.
//!
//! Aggregation, the shareable card render, and the public claim link land here
//! in the feature phase. This is the scaffold the build-time layer targets.

/// Crate identifier, surfaced in the server `/v1/health` Pro section of signed
/// builds so we can confirm the Pro layer was actually linked.
pub const FEATURE: &str = "wrapple";

/// Build version of the linked Pro wrapple crate.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn feature_id_is_stable() {
        // Must match the server `Feature::Wrapple.as_str()` and the desktop
        // `FeatureKey` so entitlement checks line up across repos.
        assert_eq!(super::FEATURE, "wrapple");
    }
}
