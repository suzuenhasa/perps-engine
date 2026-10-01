//! The key registry: each account's public key, loaded once when the gateways start, from
//! a text file, `keys.txt` (`docs/PIPELINE.md` 5.4; `docs/DECISIONS.md` D-021).
//!
//! **The file.**
//!
//! ```text
//! # perps key registry v1
//! deployment 1
//! 1 <66 hex digits>
//! 2 <66 hex digits>
//! 9 03872ba80a104a5c56609998c3da5e267e380deffe94d6e3d024969cb2e0efb729
//! ```
//!
//! The header comment, the deployment id, then one line per account: the account id in
//! decimal, one space, and the public key as a 33-byte compressed SEC1 point in 66 hex
//! digits. Lines are sorted by account, with no duplicates.
//!
//! **Contract.** [`KeyRegistry::load`] loads the file for one signature verifier (`k256`,
//! or libsecp256k1 in a build with the `c-secp256k1` feature; `verifier.rs`). It refuses a
//! verifier the build doesn't have before it reads anything, and it refuses the whole file,
//! naming the line, if the first line is not the header comment, the deployment differs
//! from the gateway's, a line is not `<account> <key>` in exactly that form, the accounts
//! are not strictly increasing (unsorted, or a duplicate), a key is not a point on the
//! curve, or the file lists account 4,294,967,295, the insurance fund (`FUND`), which can't
//! trade. A registry is never partly loaded.
//!
//! **One spelling.** Every rule above names one form, and the loader also checks the whole
//! file against it: the file must be exactly what [`registry_text`] writes for the keys it
//! lists, so `\n` line endings (not `\r\n`), a newline after the last line, and lower-case
//! hex. So one set of keys has one file, and one digest, and an auditor who rebuilds the
//! file from the keys the clients confirm (13.4) gets the digest the journal carries.
//!
//! **Parsed once.** Parsing a compressed key costs a square root in the field, so it is done
//! here, at load, for the verifier the registry is loaded for, never per message. Each
//! gateway then keeps the parsed keys of its own accounts only ([`KeyRegistry::partition`],
//! the routing rule of 6.2); the "verify on core" ablation and the audit use the same
//! parsed keys, so everything that verifies with a registry uses its verifier.
//!
//! **The digest.** [`KeyRegistry::digest`] is the SHA-256 of the file's bytes as loaded.
//! It goes into the header of every journal segment this process writes (11.2), so the
//! signature audit (13.4) can tell which registry file applied to which records, also when
//! a key was replaced at a restart. Keys change only at a restart; the engine never sees
//! them, and they are not journaled.
//!
//! **Writing.** [`registry_text`] and [`write_registry`] produce the file (the load
//! generator writes it from its seed, 14.8), sorted, so that what is written loads back.
//!
//! **Complexity.** Loading: one point decompression per account. Looking up a key:
//! O(log accounts).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};

use engine::engine::FUND;
use engine::types::AccountId;
use k256::ecdsa::VerifyingKey;
use k256::sha2::{Digest, Sha256};

use crate::gateway_of;
use crate::verifier::{KEY_BYTES, NotBuilt, PublicKey, VerifierKind};

/// The first line of every registry file.
pub const HEADER_LINE: &str = "# perps key registry v1";

/// The loaded registry. See the module docs.
#[derive(Clone)]
pub struct KeyRegistry {
    deployment: u32,
    /// The verifier every key below was parsed for.
    verifier: VerifierKind,
    keys: BTreeMap<AccountId, PublicKey>,
    digest: [u8; 32],
}

impl KeyRegistry {
    /// Loads `keys.txt` from `path` for deployment `deployment`, parsing its keys for
    /// `verifier` (module docs).
    pub fn load(path: &Path, deployment: u32, verifier: VerifierKind) -> Result<KeyRegistry, RegistryError> {
        verifier.check_built()?;
        let bytes =
            std::fs::read(path).map_err(|error| RegistryError::Io { path: path.to_path_buf(), error })?;
        KeyRegistry::parse(&bytes, deployment, verifier)
    }

    /// Parses the bytes of a registry file for deployment `deployment`, and its keys for
    /// `verifier` (module docs).
    pub fn parse(
        bytes: &[u8],
        deployment: u32,
        verifier: VerifierKind,
    ) -> Result<KeyRegistry, RegistryError> {
        verifier.check_built()?;
        let text = std::str::from_utf8(bytes).map_err(|_| RegistryError::NotText)?;
        let mut lines = text.lines().zip(1..);
        match lines.next() {
            Some((HEADER_LINE, _)) => {}
            _ => return Err(RegistryError::line(1, format!("expected the header line {HEADER_LINE:?}"))),
        }
        let file_deployment = lines
            .next()
            .and_then(|(line, _)| parse_decimal(line.strip_prefix("deployment ")?))
            .ok_or_else(|| RegistryError::line(2, "expected `deployment <id>`"))?;
        if file_deployment != deployment {
            return Err(RegistryError::WrongDeployment { file: file_deployment, expected: deployment });
        }
        let mut keys = BTreeMap::new();
        let mut previous: Option<AccountId> = None;
        for (line, number) in lines {
            let (account, key) =
                parse_account_line(line, verifier).map_err(|what| RegistryError::line(number, what))?;
            if let Some(previous) = previous.filter(|&previous| account <= previous) {
                let what = if account == previous { "a duplicate of the line before" } else { "not sorted" };
                return Err(RegistryError::line(number, format!("account {account} is {what}")));
            }
            previous = Some(account);
            keys.insert(account, key);
        }
        check_one_spelling(text, deployment, &keys)?;
        Ok(KeyRegistry { deployment, verifier, keys, digest: Sha256::digest(bytes).into() })
    }

    /// The registry of `keys` for `deployment`, exactly as [`write_registry`] would write
    /// and [`KeyRegistry::load`] read it back for `verifier`, with the same checks and
    /// digest.
    pub fn from_keys(
        deployment: u32,
        keys: &[(AccountId, VerifyingKey)],
        verifier: VerifierKind,
    ) -> Result<KeyRegistry, RegistryError> {
        KeyRegistry::parse(registry_text(deployment, keys).as_bytes(), deployment, verifier)
    }

    /// The deployment the file names.
    pub fn deployment(&self) -> u32 {
        self.deployment
    }

    /// The verifier the keys were parsed for.
    pub fn verifier(&self) -> VerifierKind {
        self.verifier
    }

    /// SHA-256 of the file's bytes (module docs, "The digest").
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Accounts with a key.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The registered key of `account`, if it has one.
    pub fn key(&self, account: AccountId) -> Option<&PublicKey> {
        self.keys.get(&account)
    }

    /// The accounts of gateway `index` of `count` (`account mod N == g`, 6.2) and their
    /// keys, in account order.
    pub fn partition(&self, index: usize, count: usize) -> impl Iterator<Item = (AccountId, &PublicKey)> {
        self.keys
            .iter()
            .map(|(&account, key)| (account, key))
            .filter(move |&(account, _)| gateway_of(account, count) == index)
    }
}

impl fmt::Debug for KeyRegistry {
    /// The deployment, the verifier, the number of keys and the digest: not every key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRegistry")
            .field("deployment", &self.deployment)
            .field("verifier", &self.verifier)
            .field("accounts", &self.keys.len())
            .field("digest", &hex(&self.digest))
            .finish()
    }
}

/// The text of a registry file for `deployment` listing `keys`, sorted by account (the
/// module docs' format). Duplicates and the `FUND` account are written as given; loading
/// refuses them.
pub fn registry_text(deployment: u32, keys: &[(AccountId, VerifyingKey)]) -> String {
    let mut sorted: Vec<(AccountId, [u8; KEY_BYTES])> =
        keys.iter().map(|(account, key)| (*account, PublicKey::K256(*key).to_compressed())).collect();
    sorted.sort_by_key(|(account, _)| *account);
    text_of(deployment, sorted)
}

/// The text of a registry file for `deployment` listing `keys` (compressed) in the order
/// given.
fn text_of(deployment: u32, keys: impl IntoIterator<Item = (AccountId, [u8; KEY_BYTES])>) -> String {
    let mut text = format!("{HEADER_LINE}\ndeployment {deployment}\n");
    for (account, key) in keys {
        writeln!(text, "{account} {}", hex(&key)).expect("writing to a String can't fail");
    }
    text
}

/// Writes the registry file for `deployment` listing `keys` to `path`, and returns its
/// digest (the one the journal's headers will carry).
pub fn write_registry(
    path: &Path,
    deployment: u32,
    keys: &[(AccountId, VerifyingKey)],
) -> io::Result<[u8; 32]> {
    let text = registry_text(deployment, keys);
    std::fs::write(path, &text)?;
    Ok(Sha256::digest(text.as_bytes()).into())
}

/// Why a registry file was refused.
#[derive(Debug)]
pub enum RegistryError {
    /// The file couldn't be read.
    Io { path: PathBuf, error: io::Error },
    /// The file is not UTF-8 text.
    NotText,
    /// Line `line` (counting from 1) is wrong.
    Line { line: usize, what: String },
    /// The file is for another deployment than the gateway's.
    WrongDeployment { file: u32, expected: u32 },
    /// The keys were to be parsed for a verifier this build doesn't have.
    NotBuilt(NotBuilt),
}

impl RegistryError {
    fn line(line: usize, what: impl Into<String>) -> RegistryError {
        RegistryError::Line { line, what: what.into() }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Io { path, error } => {
                write!(f, "reading the key registry {}: {error}", path.display())
            }
            RegistryError::NotText => f.write_str("the key registry is not UTF-8 text"),
            RegistryError::Line { line, what } => write!(f, "key registry, line {line}: {what}"),
            RegistryError::WrongDeployment { file, expected } => {
                write!(f, "the key registry is for deployment {file}, but this deployment is {expected}")
            }
            RegistryError::NotBuilt(not_built) => write!(f, "loading the key registry: {not_built}"),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<NotBuilt> for RegistryError {
    fn from(not_built: NotBuilt) -> RegistryError {
        RegistryError::NotBuilt(not_built)
    }
}

/// The file must be exactly what [`registry_text`] writes for the keys it lists (module
/// docs, "One spelling"). `str::lines` also takes `\r\n` and a last line without a newline,
/// and hex digits parse in either case, so without this check the same keys could load
/// from several files, each with its own digest (review finding F1). The error names the
/// first line that differs.
fn check_one_spelling(
    text: &str,
    deployment: u32,
    keys: &BTreeMap<AccountId, PublicKey>,
) -> Result<(), RegistryError> {
    // The map is sorted by account, as `registry_text` sorts.
    let canonical = text_of(deployment, keys.iter().map(|(&account, key)| (account, key.to_compressed())));
    if text == canonical {
        return Ok(());
    }
    // Both agree up to `same` bytes, all ASCII, so `same` is a character boundary of `text`.
    let same = text.bytes().zip(canonical.bytes()).take_while(|(a, b)| a == b).count();
    let line = text[..same].matches('\n').count() + 1;
    Err(RegistryError::line(
        line,
        "not in the one form registries are written in: `\\n` line endings, a newline after the last \
         line, lower-case hex",
    ))
}

/// One account line: `<account in decimal> <66 hex digits>`, its key parsed for `verifier`.
fn parse_account_line(line: &str, verifier: VerifierKind) -> Result<(AccountId, PublicKey), String> {
    let (account, key) = line.split_once(' ').ok_or("expected `<account> <66 hex digits>`")?;
    let number: u32 = parse_decimal(account).ok_or(format!("{account:?} is not an account id"))?;
    let account = AccountId::new(number);
    if account == FUND {
        return Err(format!("account {FUND} is the insurance fund, which can't trade"));
    }
    let key: [u8; KEY_BYTES] =
        parse_hex(key).ok_or(format!("account {account}: the key is not 66 hex digits"))?;
    let key = PublicKey::from_compressed(verifier, &key)
        .ok_or_else(|| format!("account {account}: the key is not a compressed point on secp256k1"))?;
    Ok((account, key))
}

/// A number in plain decimal: digits only, no sign, no leading zero (so each number has one
/// spelling, and the digest one file).
fn parse_decimal<T: std::str::FromStr>(text: &str) -> Option<T> {
    let digits_only = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let leading_zero = text.len() > 1 && text.starts_with('0');
    if digits_only && !leading_zero { text.parse().ok() } else { None }
}

/// Exactly `2 * N` hex digits. (`u8::from_str_radix` alone would also take a `+` sign.)
fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Lower-case hex digits of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ACCOUNT_9_PUBLIC_KEY, VERIFIER, acct, public_key, signing_key};

    fn key(account: AccountId) -> VerifyingKey {
        *signing_key(1, account).verifying_key()
    }

    /// Account `n` with its key.
    fn keyed(n: u32) -> (AccountId, VerifyingKey) {
        (acct(n), key(acct(n)))
    }

    fn good_text() -> String {
        registry_text(1, &[keyed(9), keyed(1), keyed(2)])
    }

    fn parse(text: &str, deployment: u32) -> Result<KeyRegistry, RegistryError> {
        KeyRegistry::parse(text.as_bytes(), deployment, VERIFIER)
    }

    fn error_of(text: &str) -> String {
        parse(text, 1).expect_err("refused").to_string()
    }

    #[test]
    fn a_good_file_loads_and_is_sorted_and_digested() {
        let text = good_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..2], [HEADER_LINE, "deployment 1"]);
        assert!(lines[2].starts_with("1 ") && lines[3].starts_with("2 "), "sorted: {text}");
        assert_eq!(lines[4], format!("9 {ACCOUNT_9_PUBLIC_KEY}"), "the key of 5.4 and 5.6");

        let registry = parse(&text, 1).expect("loads");
        assert_eq!((registry.deployment(), registry.len(), registry.verifier()), (1, 3, VERIFIER));
        assert_eq!(registry.key(acct(9)), Some(&public_key(&signing_key(1, acct(9)))));
        assert_eq!(registry.key(acct(3)), None);
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        assert_eq!(registry.digest(), digest);
        let keys = [keyed(1), keyed(2), keyed(9)];
        let from_keys = KeyRegistry::from_keys(1, &keys, VERIFIER).expect("valid");
        assert_eq!(from_keys.digest(), digest);
        assert!(format!("{registry:?}").contains(&hex(&digest)));
    }

    #[test]
    fn written_files_load_back_with_the_digest_write_returned() {
        let dir = std::env::temp_dir().join(format!("gateway-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("created");
        let path = dir.join("keys.txt");
        let digest = write_registry(&path, 5, &[keyed(4), keyed(3)]).expect("written");
        let registry = KeyRegistry::load(&path, 5, VERIFIER).expect("loads");
        assert_eq!(registry.digest(), digest);
        assert_eq!(registry.len(), 2);
        let missing = KeyRegistry::load(&dir.join("absent.txt"), 5, VERIFIER).expect_err("no such file");
        assert!(matches!(missing, RegistryError::Io { .. }), "{missing}");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_gateway_gets_the_accounts_that_route_to_it() {
        let accounts = [1, 2, 3, 4, 9, 10, 17];
        let keys = accounts.map(keyed);
        let registry = KeyRegistry::from_keys(1, &keys, VERIFIER).expect("valid");
        let of = |g| registry.partition(g, 8).map(|(account, _)| account).collect::<Vec<_>>();
        assert_eq!(of(1), [1, 9, 17].map(acct));
        assert_eq!(of(2), [2, 10].map(acct));
        assert_eq!(of(0), [] as [AccountId; 0]);
        let all: usize = (0..8).map(|g| registry.partition(g, 8).count()).sum();
        assert_eq!(all, accounts.len(), "every account on exactly one gateway");
    }

    #[test]
    fn a_duplicate_or_unsorted_line_is_refused() {
        let text = good_text();
        let lines: Vec<&str> = text.lines().collect();
        let duplicate = format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[2], lines[2]);
        assert!(
            error_of(&duplicate).contains("line 4: account 1 is a duplicate"),
            "{}",
            error_of(&duplicate)
        );
        let unsorted = format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[3], lines[2]);
        assert!(error_of(&unsorted).contains("line 4: account 1 is not sorted"), "{}", error_of(&unsorted));
    }

    #[test]
    fn a_key_that_is_not_a_point_on_the_curve_is_refused() {
        let with_key = |key: &str| format!("{HEADER_LINE}\ndeployment 1\n7 {key}\n");
        // x = 5: 5^3 + 7 = 132 is not a square modulo p, so no point has this x.
        let off_curve = format!("02{:064x}", 5);
        // x = 2^256 − 1 is not below p.
        let too_big = format!("03{}", "ff".repeat(32));
        // 0x04 means an uncompressed point, which needs 65 bytes.
        let wrong_tag = format!("04{}", &ACCOUNT_9_PUBLIC_KEY[2..]);
        // 0x05 is SEC1's "compact" form (`x` alone), which `k256` alone would parse (5.7).
        let compact = format!("05{}", &ACCOUNT_9_PUBLIC_KEY[2..]);
        for key in [off_curve, too_big, wrong_tag, compact] {
            let error = error_of(&with_key(&key));
            assert!(error.contains("line 3: account 7: the key is not a compressed point"), "{key}: {error}");
        }
        let short = error_of(&with_key(&ACCOUNT_9_PUBLIC_KEY[..64]));
        assert!(short.contains("not 66 hex digits"), "{short}");
        let signed = error_of(&with_key(&format!("+3{}", &ACCOUNT_9_PUBLIC_KEY[2..])));
        assert!(signed.contains("not 66 hex digits"), "{signed}");
    }

    #[test]
    fn another_spelling_of_the_same_keys_is_refused() {
        let text = good_text();
        let upper_hex = text
            .lines()
            .map(|line| {
                format!("{}\n", if line.starts_with("9 ") { line.to_uppercase() } else { line.to_string() })
            })
            .collect::<String>();
        let cases = [
            (text.replace('\n', "\r\n"), "line 1"),
            (text.trim_end_matches('\n').to_string(), "line 5"),
            (upper_hex, "line 5"),
        ];
        for (other, line) in cases {
            let error = error_of(&other);
            assert!(error.contains(&format!("{line}: not in the one form")), "{other:?}: {error}");
        }
        assert!(parse(&text, 1).is_ok(), "the written form loads");
    }

    #[test]
    fn the_insurance_fund_is_refused() {
        let text = format!("{HEADER_LINE}\ndeployment 1\n{FUND} {ACCOUNT_9_PUBLIC_KEY}\n");
        assert!(
            error_of(&text).contains("line 3: account 4294967295 is the insurance fund"),
            "{}",
            error_of(&text)
        );
        let keys = [(FUND, key(acct(9)))];
        assert!(KeyRegistry::from_keys(1, &keys, VERIFIER).is_err());
    }

    #[test]
    fn a_file_for_another_deployment_is_refused() {
        let error = parse(&good_text(), 2).expect_err("deployment 1, not 2");
        assert!(matches!(error, RegistryError::WrongDeployment { file: 1, expected: 2 }), "{error}");
    }

    #[test]
    fn a_line_in_any_other_form_is_refused() {
        let key9 = ACCOUNT_9_PUBLIC_KEY;
        let cases = [
            ("deployment 1\n".to_string(), "line 1: expected the header line"),
            (format!("{HEADER_LINE}\n"), "line 2: expected `deployment <id>`"),
            (format!("{HEADER_LINE}\ndeployment 01\n"), "line 2"),
            (format!("{HEADER_LINE}\ndeployment 1\n\n"), "line 3: expected `<account> <66 hex digits>`"),
            (format!("{HEADER_LINE}\ndeployment 1\n09 {key9}\n"), "line 3: \"09\" is not an account id"),
            (format!("{HEADER_LINE}\ndeployment 1\n+9 {key9}\n"), "line 3: \"+9\" is not an account id"),
            (format!("{HEADER_LINE}\ndeployment 1\n9  {key9}\n"), "line 3: account 9: the key is not 66"),
            (format!("{HEADER_LINE}\ndeployment 1\n4294967296 {key9}\n"), "line 3"),
        ];
        for (text, expected) in cases {
            let error = error_of(&text);
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        assert!(matches!(KeyRegistry::parse(&[0xFF, 0xFE], 1, VERIFIER), Err(RegistryError::NotText)));
        // An empty registry is a valid file: no account can trade.
        let empty = parse(&format!("{HEADER_LINE}\ndeployment 1\n"), 1).expect("valid");
        assert!(empty.is_empty());
    }

    #[test]
    fn a_verifier_the_build_does_not_have_is_refused_before_the_file_is_read() {
        let text = good_text();
        let libsecp = KeyRegistry::parse(text.as_bytes(), 1, VerifierKind::LibSecp256k1);
        let missing = std::env::temp_dir().join(format!("gateway-registry-absent-{}", std::process::id()));
        let libsecp_load = KeyRegistry::load(&missing, 1, VerifierKind::LibSecp256k1);
        if cfg!(feature = "c-secp256k1") {
            let registry = libsecp.expect("loads");
            assert_eq!(registry.verifier(), VerifierKind::LibSecp256k1);
            assert!(matches!(libsecp_load, Err(RegistryError::Io { .. })), "{libsecp_load:?}");
        } else {
            for refused in [libsecp.expect_err("not built"), libsecp_load.expect_err("not built")] {
                assert!(matches!(refused, RegistryError::NotBuilt(_)), "{refused:?}");
                assert!(refused.to_string().contains("`--features c-secp256k1`"), "{refused}");
            }
        }
        // Either way, the same file loads for k256, with the same digest.
        let k256 = KeyRegistry::parse(text.as_bytes(), 1, VerifierKind::K256).expect("loads");
        assert_eq!(k256.digest(), parse(&text, 1).expect("loads").digest());
    }
}
