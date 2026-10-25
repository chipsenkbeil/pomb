use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::{RngCore, thread_rng};
use webauthn_rs::prelude::{
    CreationChallengeResponse, CredentialID, DiscoverableAuthentication, DiscoverableKey, Passkey,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential, Url, Uuid, Webauthn,
    WebauthnBuilder, WebauthnError,
};

use crate::auth;
use crate::models::{PasskeyRow, UserRow};

/// Display name the relying party gives authenticators.
const RP_NAME: &str = "Pomb";

/// Seconds a started ceremony stays usable.
pub const CEREMONY_TTL_SECONDS: i64 = 300;

/// Wrong setup codes, across all clients, after which the code is replaced by a fresh one.
pub const MAX_SETUP_CODE_GUESSES: usize = 10;

/// A started ceremony's id and the options the browser passes to WebAuthn.
type StartResult = Result<(String, serde_json::Value), Box<dyn std::error::Error>>;

/// Why a ceremony could not finish.
#[derive(Debug)]
pub enum PasskeyError {
    /// The ceremony id is unknown, already used, expired, or of another kind.
    UnknownCeremony,
    /// The credential names no stored passkey of the user it claims.
    UnknownCredential,
    /// The relying party rejected the credential.
    Rejected(WebauthnError),
}

impl fmt::Display for PasskeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCeremony => f.write_str("the ceremony is unknown, used or expired"),
            Self::UnknownCredential => f.write_str("the passkey is not registered here"),
            Self::Rejected(error) => write!(f, "the passkey was rejected: {error}"),
        }
    }
}

impl std::error::Error for PasskeyError {}

/// What a registration ceremony adds a passkey for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Enrollment {
    /// The instance's first passkey, which claims it.
    First,
    /// Another passkey for the signed-in owner.
    Another,
}

/// A finished registration: the user it was started for and the new credential.
#[derive(Debug)]
pub struct Registered {
    /// The user the ceremony was started for, which may not be stored yet.
    pub user: UserRow,
    /// The verified new credential.
    pub credential: Passkey,
}

/// A finished sign-in: the stored passkey used, and its credential with the counter and backup
/// flags the sign-in reported.
#[derive(Debug)]
pub struct SignedIn {
    /// The stored passkey the browser signed in with.
    pub passkey: PasskeyRow,
    /// Its credential, updated by this sign-in.
    pub credential: Passkey,
}

/// The WebAuthn relying party behind the public URL, with its pending ceremonies.
#[derive(Clone, Debug)]
pub struct RelyingParty {
    /// The webauthn-rs relying party.
    webauthn: Arc<Webauthn>,
    /// The RP ID, the public URL's host.
    host: String,
    /// Started sign-ins, waiting for their finish.
    sign_ins: Pool<DiscoverableAuthentication>,
    /// Started registrations, waiting for their finish, kept apart so sign-ins cannot crowd them
    /// out.
    registrations: Pool<Enrolling>,
    /// The code that guards the first passkey's registration.
    setup_code: SetupCode,
}

impl RelyingParty {
    /// Builds the relying party for `public_url`: its host is the RP ID and its scheme, host and
    /// port the only accepted origin.
    pub fn new(public_url: &str) -> Result<Self, String> {
        let origin = Url::parse(public_url).map_err(|error| error.to_string())?;
        let Some(host) = origin.domain().map(str::to_string) else {
            return Err("has no DNS name, which passkeys need as their RP ID".to_string());
        };
        let webauthn = WebauthnBuilder::new(&host, &origin)
            .and_then(|builder| builder.rp_name(RP_NAME).build())
            .map_err(|error| format!("cannot serve as the passkey relying party: {error}"))?;
        Ok(Self {
            webauthn: Arc::new(webauthn),
            host,
            sign_ins: Pool::default(),
            registrations: Pool::default(),
            setup_code: SetupCode::default(),
        })
    }

    /// Issues a fresh setup code that replaces any earlier one, and returns it as people type it.
    pub fn issue_setup_code(&self) -> String {
        self.setup_code.issue()
    }

    /// Issues a fresh setup code and prints it for the operator.
    pub fn announce_setup_code(&self) {
        let code = self.issue_setup_code();
        println!("pomb: setup code {code} (enter it at /setup to register the first passkey)");
    }

    /// Returns whether `typed` is the live setup code, in any case and with or without its dash.
    ///
    /// The [`MAX_SETUP_CODE_GUESSES`]th wrong code replaces the code with a fresh, printed one.
    pub fn setup_code_matches(&self, typed: &str) -> bool {
        match self.setup_code.check(typed) {
            Guess::Right => true,
            Guess::Wrong => false,
            Guess::Exhausted => {
                self.announce_setup_code();
                false
            }
        }
    }

    /// Spends the setup code, so it cannot register another passkey.
    pub fn burn_setup_code(&self) {
        self.setup_code.burn();
    }

    /// Returns the RP ID, the public URL's host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Starts registering a passkey for `user`, refusing the credentials in `exclude`.
    ///
    /// The returned options ask for a discoverable credential, since sign-in names no user.
    pub fn start_registration(
        &self,
        user: &UserRow,
        enrollment: Enrollment,
        exclude: Vec<CredentialID>,
        now: i64,
    ) -> StartResult {
        let handle = Uuid::parse_str(&user.id)?;
        let exclude = (!exclude.is_empty()).then_some(exclude);
        let (options, state) = self.webauthn.start_passkey_registration(
            handle,
            &user.username,
            &user.username,
            exclude,
        )?;
        let id = self.registrations.insert(
            Enrolling {
                user: user.clone(),
                enrollment,
                state,
            },
            now,
        );
        Ok((id, require_discoverable(&options)?))
    }

    /// Finishes registration ceremony `id`, which must have been started for `enrollment`.
    pub fn finish_registration(
        &self,
        id: &str,
        enrollment: Enrollment,
        credential: &RegisterPublicKeyCredential,
        now: i64,
    ) -> Result<Registered, PasskeyError> {
        let Some(Enrolling {
            user,
            enrollment: started,
            state,
        }) = self.registrations.take(id, now)
        else {
            return Err(PasskeyError::UnknownCeremony);
        };
        if started != enrollment {
            return Err(PasskeyError::UnknownCeremony);
        }
        let credential = self
            .webauthn
            .finish_passkey_registration(credential, &state)
            .map_err(PasskeyError::Rejected)?;
        Ok(Registered { user, credential })
    }

    /// Starts a sign-in with a discoverable passkey, so the browser offers every passkey it holds
    /// for this site.
    pub fn start_sign_in(&self, now: i64) -> StartResult {
        let (mut options, state) = self.webauthn.start_discoverable_authentication()?;
        options.mediation = None;
        let id = self.sign_ins.insert(state, now);
        Ok((id, serde_json::to_value(&options)?))
    }

    /// Finishes sign-in ceremony `id` against the stored `passkeys`.
    pub fn finish_sign_in(
        &self,
        id: &str,
        credential: &PublicKeyCredential,
        passkeys: Vec<PasskeyRow>,
        now: i64,
    ) -> Result<SignedIn, PasskeyError> {
        let Some(state) = self.sign_ins.take(id, now) else {
            return Err(PasskeyError::UnknownCeremony);
        };
        let (handle, credential_id) = self
            .webauthn
            .identify_discoverable_authentication(credential)
            .map_err(PasskeyError::Rejected)?;
        let handle = handle.to_string();
        let (passkey, mut stored) = passkeys
            .into_iter()
            .filter(|row| row.user_id == handle)
            .find_map(|row| {
                let stored = row.passkey().ok()?;
                (stored.cred_id().as_slice() == credential_id).then_some((row, stored))
            })
            .ok_or(PasskeyError::UnknownCredential)?;
        let result = self
            .webauthn
            .finish_discoverable_authentication(
                credential,
                state,
                &[DiscoverableKey::from(&stored)],
            )
            .map_err(PasskeyError::Rejected)?;
        stored.update_credential(&result);
        Ok(SignedIn {
            passkey,
            credential: stored,
        })
    }
}

/// Serializes registration options with a required resident key, which makes the credential
/// discoverable.
fn require_discoverable(
    options: &CreationChallengeResponse,
) -> serde_json::Result<serde_json::Value> {
    let mut value = serde_json::to_value(options)?;
    if let Some(selection) = value
        .pointer_mut("/publicKey/authenticatorSelection")
        .and_then(serde_json::Value::as_object_mut)
    {
        selection.insert("residentKey".to_string(), "required".into());
        selection.insert("requireResidentKey".to_string(), true.into());
    }
    Ok(value)
}

/// How a typed setup code compared with the live one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Guess {
    /// The live code.
    Right,
    /// Not the live code, or no code is live.
    Wrong,
    /// Wrong, and the last of the guesses the code allows.
    Exhausted,
}

/// The live setup code and the wrong guesses made against it.
#[derive(Debug, Default)]
struct SetupState {
    /// The code in canonical form, absent once burned or before one is issued.
    code: Option<String>,
    /// Wrong codes typed since the code was issued.
    wrong: usize,
}

/// The setup code, held in memory until the first passkey is registered.
#[derive(Clone, Debug, Default)]
struct SetupCode {
    /// The code and its wrong guesses.
    state: Arc<Mutex<SetupState>>,
}

impl SetupCode {
    /// Locks the code.
    fn lock(&self) -> MutexGuard<'_, SetupState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Replaces the code with a fresh one and returns it as people type it.
    fn issue(&self) -> String {
        let code = auth::generate_user_code();
        *self.lock() = SetupState {
            code: Some(code.clone()),
            wrong: 0,
        };
        auth::display_user_code(&code)
    }

    /// Compares `typed` with the live code in constant time, counting a wrong guess.
    fn check(&self, typed: &str) -> Guess {
        let typed = auth::normalize_user_code(typed);
        let mut state = self.lock();
        let Some(code) = state.code.as_deref() else {
            return Guess::Wrong;
        };
        if auth::constant_time_eq(typed.as_bytes(), code.as_bytes()) {
            return Guess::Right;
        }
        state.wrong += 1;
        if state.wrong >= MAX_SETUP_CODE_GUESSES {
            Guess::Exhausted
        } else {
            Guess::Wrong
        }
    }

    /// Removes the code, so no typed code matches.
    fn burn(&self) {
        self.lock().code = None;
    }
}

/// A registration waiting for its finish, with the server-side state the browser never sees.
#[derive(Debug)]
struct Enrolling {
    /// The user the passkey is for.
    user: UserRow,
    /// What the registration adds the passkey for.
    enrollment: Enrollment,
    /// The webauthn-rs registration state.
    state: PasskeyRegistration,
}

/// A started ceremony and when it started.
#[derive(Debug)]
struct Pending<T> {
    /// Unix seconds.
    started_at: i64,
    /// The server-side state the finish needs.
    state: T,
}

/// Started ceremonies of one kind, kept in memory until they finish or expire. Each is usable
/// once, and past [`Pool::MAX_PENDING`] the oldest is dropped to make room.
#[derive(Debug)]
struct Pool<T> {
    /// Ceremonies by id.
    pending: Arc<Mutex<HashMap<String, Pending<T>>>>,
}

impl<T> Clone for Pool<T> {
    fn clone(&self) -> Self {
        Self {
            pending: Arc::clone(&self.pending),
        }
    }
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Self {
            pending: Arc::default(),
        }
    }
}

impl<T> Pool<T> {
    /// Most ceremonies of one kind that may wait at once, far above what the per-IP limits allow
    /// within [`CEREMONY_TTL_SECONDS`].
    const MAX_PENDING: usize = 10_000;

    /// Stores `state` under a fresh ceremony id and returns the id.
    fn insert(&self, state: T, now: i64) -> String {
        let mut pending = self.live(now);
        if pending.len() >= Self::MAX_PENDING {
            let oldest = pending
                .iter()
                .min_by_key(|(_, entry)| entry.started_at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                pending.remove(&id);
            }
        }
        let id = new_ceremony_id();
        pending.insert(
            id.clone(),
            Pending {
                started_at: now,
                state,
            },
        );
        id
    }

    /// Removes and returns the state of ceremony `id`, if it is still live.
    fn take(&self, id: &str, now: i64) -> Option<T> {
        self.live(now).remove(id).map(|entry| entry.state)
    }

    /// Locks the pool after dropping the expired ceremonies.
    fn live(&self, now: i64) -> MutexGuard<'_, HashMap<String, Pending<T>>> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.retain(|_, entry| now - entry.started_at < CEREMONY_TTL_SECONDS);
        pending
    }
}

/// A ceremony id: 32 random bytes as base64url.
fn new_ceremony_id() -> String {
    let mut raw = [0u8; 32];
    thread_rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn relying_party() -> RelyingParty {
        RelyingParty::new("https://pomb.test").expect("relying party")
    }

    fn owner() -> UserRow {
        UserRow {
            id: Uuid::new_v4().to_string(),
            username: "chip".to_string(),
        }
    }

    fn assertion() -> PublicKeyCredential {
        serde_json::from_value(serde_json::json!({
            "id": "AAAA",
            "rawId": "AAAA",
            "type": "public-key",
            "response": {
                "clientDataJSON": "e30",
                "authenticatorData": "AAAA",
                "signature": "AAAA",
                "userHandle": URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes()),
            },
            "extensions": {},
        }))
        .expect("credential")
    }

    fn registration_credential() -> RegisterPublicKeyCredential {
        serde_json::from_value(serde_json::json!({
            "id": "AAAA",
            "rawId": "AAAA",
            "type": "public-key",
            "response": { "clientDataJSON": "e30", "attestationObject": "oA" },
            "extensions": {},
        }))
        .expect("credential")
    }

    fn finish(relying_party: &RelyingParty, id: &str, now: i64) -> Result<SignedIn, PasskeyError> {
        relying_party.finish_sign_in(id, &assertion(), Vec::new(), now)
    }

    #[test]
    fn ceremony_should_expire_after_its_ttl() {
        let relying_party = relying_party();
        let (id, _) = relying_party.start_sign_in(NOW).expect("start");
        let expired = finish(&relying_party, &id, NOW + CEREMONY_TTL_SECONDS);
        assert!(matches!(expired, Err(PasskeyError::UnknownCeremony)));

        let (id, _) = relying_party.start_sign_in(NOW).expect("start");
        let live = finish(&relying_party, &id, NOW + CEREMONY_TTL_SECONDS - 1);
        assert!(matches!(live, Err(PasskeyError::UnknownCredential)));
    }

    #[test]
    fn ceremony_should_be_usable_once() {
        let relying_party = relying_party();
        let (id, _) = relying_party.start_sign_in(NOW).expect("start");
        assert!(matches!(
            finish(&relying_party, &id, NOW),
            Err(PasskeyError::UnknownCredential)
        ));
        assert!(matches!(
            finish(&relying_party, &id, NOW),
            Err(PasskeyError::UnknownCeremony)
        ));
    }

    #[test]
    fn pool_should_drop_the_oldest_past_its_cap() {
        let pool = Pool::<usize>::default();
        let oldest = pool.insert(0, NOW);
        let ids: Vec<String> = (1..=Pool::<usize>::MAX_PENDING)
            .map(|n| pool.insert(n, NOW + 1))
            .collect();
        assert_eq!(pool.live(NOW + 1).len(), Pool::<usize>::MAX_PENDING);
        assert_eq!(pool.take(&oldest, NOW + 1), None);
        assert_eq!(pool.take(&ids[0], NOW + 1), Some(1));
    }

    #[test]
    fn sign_in_flood_should_not_evict_a_pending_registration() {
        let relying_party = relying_party();
        let (registration, _) = relying_party
            .start_registration(&owner(), Enrollment::Another, Vec::new(), NOW)
            .expect("start");
        for _ in 0..=Pool::<usize>::MAX_PENDING {
            relying_party.start_sign_in(NOW).expect("start");
        }
        let finished = relying_party.finish_registration(
            &registration,
            Enrollment::Another,
            &registration_credential(),
            NOW,
        );
        assert!(matches!(finished, Err(PasskeyError::Rejected(_))));
    }

    #[test]
    fn setup_code_should_be_replaced_after_too_many_wrong_guesses() {
        let relying_party = relying_party();
        let code = relying_party.issue_setup_code();
        for _ in 1..MAX_SETUP_CODE_GUESSES {
            assert!(!relying_party.setup_code_matches("BCDF-GHJK"));
        }
        assert!(relying_party.setup_code_matches(&code));
        assert!(!relying_party.setup_code_matches("BCDF-GHJK"));
        assert!(!relying_party.setup_code_matches(&code));
        relying_party.burn_setup_code();
        assert!(!relying_party.setup_code_matches(&code));
    }

    #[test]
    fn registration_should_finish_only_for_the_enrollment_it_started() {
        let relying_party = relying_party();
        let (id, options) = relying_party
            .start_registration(&owner(), Enrollment::Another, Vec::new(), NOW)
            .expect("start");
        assert_eq!(
            options["publicKey"]["authenticatorSelection"]["residentKey"],
            "required"
        );
        let wrong = relying_party.finish_registration(
            &id,
            Enrollment::First,
            &registration_credential(),
            NOW,
        );
        assert!(matches!(wrong, Err(PasskeyError::UnknownCeremony)));
        let reused = relying_party.finish_registration(
            &id,
            Enrollment::Another,
            &registration_credential(),
            NOW,
        );
        assert!(matches!(reused, Err(PasskeyError::UnknownCeremony)));
    }

    #[test]
    fn relying_party_should_need_a_dns_name() {
        assert!(RelyingParty::new("http://192.168.1.10:3000").is_err());
        assert_eq!(
            RelyingParty::new("https://pomb.example.com:8443")
                .expect("relying party")
                .host(),
            "pomb.example.com"
        );
    }
}
