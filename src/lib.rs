//! Double-entry journal.
//!
//! Amounts are minor units. A transaction is appended only when the debits
//! equal the credits. Account balances are debits minus credits, so the sum
//! of every account is zero.
//!
//! One process writes the file. There is one currency, chosen by the caller,
//! and it is not stored.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"FOL1";
const MAX_KEY: usize = 128;
const MAX_MEMO: usize = 512;
const MAX_ACCOUNT: usize = 128;
const MAX_POSTINGS: usize = 1024;
const MAX_RECORD: usize = 1024 * 1024;

/// Which side of the account a posting hits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Debit,
    Credit,
}

/// One leg of a transaction. `amount` is a positive number of minor units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub account: String,
    pub side: Side,
    pub amount: u64,
}

/// A transaction that was appended. `id` starts at 1, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Txn {
    pub id: u64,
    pub key: String,
    pub memo: String,
    pub postings: Vec<Posting>,
}

/// Why `post` or `open` refused.
#[derive(Debug)]
pub enum Error {
    EmptyKey,
    BadKey,
    KeyTooLong,
    BadMemo,
    MemoTooLong,
    BadAccount,
    NoPostings,
    ZeroAmount,
    TooManyPostings,
    Unbalanced { debit: u128, credit: u128 },
    IdempotencyConflict { id: u64 },
    Overflow,
    Corrupt,
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::EmptyKey => write!(f, "empty idempotency key"),
            Error::BadKey => write!(f, "idempotency key is not a printable token"),
            Error::KeyTooLong => write!(f, "idempotency key longer than {MAX_KEY}"),
            Error::BadMemo => write!(f, "memo is not printable"),
            Error::MemoTooLong => write!(f, "memo longer than {MAX_MEMO}"),
            Error::BadAccount => write!(f, "account name is empty or not printable"),
            Error::NoPostings => write!(f, "transaction has no postings"),
            Error::ZeroAmount => write!(f, "posting amount is zero"),
            Error::TooManyPostings => write!(f, "more than {MAX_POSTINGS} postings"),
            Error::Unbalanced { debit, credit } => {
                write!(f, "unbalanced: debit {debit} credit {credit}")
            }
            Error::IdempotencyConflict { id } => {
                write!(f, "idempotency key already used by transaction {id}")
            }
            Error::Overflow => write!(f, "balance overflow"),
            Error::Corrupt => write!(f, "journal corrupt"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Append-only journal at `path`.
pub struct Journal {
    path: PathBuf,
    file: File,
    txns: Vec<Txn>,
    balances: BTreeMap<String, i128>,
    by_key: BTreeMap<String, u64>,
}

impl Journal {
    /// Open a journal, or create one. A trailing partial record is dropped.
    /// A checksum mismatch is [`Error::Corrupt`] and the file is left as it is.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            file.write_all(MAGIC)?;
            file.sync_data()?;
            bytes.extend_from_slice(MAGIC);
        }
        let parsed = parse(&bytes)?;
        if (parsed.consumed as usize) < bytes.len() {
            file.set_len(parsed.consumed)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::Start(parsed.consumed))?;
        Ok(Journal {
            path: path.to_path_buf(),
            file,
            txns: parsed.txns,
            balances: parsed.balances,
            by_key: parsed.by_key,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.txns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txns.is_empty()
    }

    pub fn txns(&self) -> &[Txn] {
        &self.txns
    }

    /// Debits minus credits. An account that was never posted is 0.
    pub fn balance(&self, account: &str) -> i128 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    /// Every account that has been posted, sorted by name, including zeroes.
    pub fn accounts(&self) -> Vec<(String, i128)> {
        self.balances.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Wrapping sum of balances. A journal this type wrote sums to 0.
    pub fn trial(&self) -> i128 {
        self.balances
            .values()
            .fold(0i128, |a, b| a.wrapping_add(*b))
    }

    /// Append a balanced transaction.
    ///
    /// `key` is the caller's idempotency key. The same key and the same body
    /// return the original id and write nothing. The same key with a different
    /// body is [`Error::IdempotencyConflict`].
    pub fn post(&mut self, key: &str, memo: &str, postings: &[Posting]) -> Result<u64, Error> {
        check_key(key)?;
        check_memo(memo)?;
        check_postings(postings)?;
        if let Some(&id) = self.by_key.get(key) {
            let old = &self.txns[(id - 1) as usize];
            if old.memo == memo && old.postings == postings {
                return Ok(id);
            }
            return Err(Error::IdempotencyConflict { id });
        }
        let mut next = self.balances.clone();
        apply(&mut next, postings)?;

        let payload = encode(key, memo, postings);
        let mut rec = Vec::with_capacity(8 + payload.len());
        rec.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        rec.extend_from_slice(&payload);
        rec.extend_from_slice(&crc32(&payload).to_le_bytes());

        let end_before = self.file.stream_position()?;
        if let Err(e) = self
            .file
            .write_all(&rec)
            .and_then(|_| self.file.sync_data())
        {
            let _ = self.file.set_len(end_before);
            let _ = self.file.seek(SeekFrom::Start(end_before));
            return Err(Error::Io(e));
        }

        let id = self.txns.len() as u64 + 1;
        self.by_key.insert(key.to_string(), id);
        self.txns.push(Txn {
            id,
            key: key.to_string(),
            memo: memo.to_string(),
            postings: postings.to_vec(),
        });
        self.balances = next;
        Ok(id)
    }
}

struct Parsed {
    txns: Vec<Txn>,
    balances: BTreeMap<String, i128>,
    by_key: BTreeMap<String, u64>,
    consumed: u64,
}

fn parse(bytes: &[u8]) -> Result<Parsed, Error> {
    if bytes.len() < 4 || &bytes[..4] != MAGIC {
        return Err(Error::Corrupt);
    }
    let mut i = 4usize;
    let mut txns = Vec::new();
    let mut balances = BTreeMap::new();
    let mut by_key = BTreeMap::new();
    while i < bytes.len() {
        if bytes.len() - i < 4 {
            break;
        }
        let len = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        let rest = bytes.len() - i;
        if len > rest - 4 || rest - 4 - len < 4 {
            break;
        }
        if len == 0 || len > MAX_RECORD {
            return Err(Error::Corrupt);
        }
        let payload = &bytes[i + 4..i + 4 + len];
        let crc = u32::from_le_bytes(bytes[i + 4 + len..i + 8 + len].try_into().unwrap());
        if crc32(payload) != crc {
            return Err(Error::Corrupt);
        }
        let (key, memo, postings) = decode(payload)?;
        if by_key.contains_key(&key) {
            return Err(Error::Corrupt);
        }
        apply(&mut balances, &postings).map_err(|_| Error::Corrupt)?;
        let id = txns.len() as u64 + 1;
        by_key.insert(key.clone(), id);
        txns.push(Txn {
            id,
            key,
            memo,
            postings,
        });
        i += 4 + len + 4;
    }
    Ok(Parsed {
        txns,
        balances,
        by_key,
        consumed: i as u64,
    })
}

fn apply(balances: &mut BTreeMap<String, i128>, postings: &[Posting]) -> Result<(), Error> {
    let mut next = balances.clone();
    for p in postings {
        let delta = match p.side {
            Side::Debit => p.amount as i128,
            Side::Credit => -(p.amount as i128),
        };
        let entry = next.entry(p.account.clone()).or_insert(0);
        *entry = entry.checked_add(delta).ok_or(Error::Overflow)?;
    }
    *balances = next;
    Ok(())
}

fn check_postings(postings: &[Posting]) -> Result<(), Error> {
    if postings.is_empty() {
        return Err(Error::NoPostings);
    }
    if postings.len() > MAX_POSTINGS {
        return Err(Error::TooManyPostings);
    }
    let mut debit = 0u128;
    let mut credit = 0u128;
    for p in postings {
        if p.amount == 0 {
            return Err(Error::ZeroAmount);
        }
        if !valid_token(&p.account, MAX_ACCOUNT) {
            return Err(Error::BadAccount);
        }
        match p.side {
            Side::Debit => debit = debit.checked_add(p.amount as u128).ok_or(Error::Overflow)?,
            Side::Credit => {
                credit = credit
                    .checked_add(p.amount as u128)
                    .ok_or(Error::Overflow)?
            }
        }
    }
    if debit != credit {
        return Err(Error::Unbalanced { debit, credit });
    }
    Ok(())
}

fn check_key(key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::EmptyKey);
    }
    if key.len() > MAX_KEY {
        return Err(Error::KeyTooLong);
    }
    if !valid_token(key, MAX_KEY) {
        return Err(Error::BadKey);
    }
    Ok(())
}

fn check_memo(memo: &str) -> Result<(), Error> {
    if memo.len() > MAX_MEMO {
        return Err(Error::MemoTooLong);
    }
    if !memo.is_empty() && !printable(memo) {
        return Err(Error::BadMemo);
    }
    Ok(())
}

fn valid_token(s: &str, max: usize) -> bool {
    let n = s.len();
    if n == 0 || n > max || s.starts_with(' ') || s.ends_with(' ') {
        return false;
    }
    printable(s)
}

fn printable(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b) || b >= 0x80)
}

fn encode(key: &str, memo: &str, postings: &[Posting]) -> Vec<u8> {
    let mut buf = Vec::new();
    push_str(&mut buf, key);
    push_str(&mut buf, memo);
    buf.extend_from_slice(&(postings.len() as u16).to_le_bytes());
    for p in postings {
        push_str(&mut buf, &p.account);
        buf.push(match p.side {
            Side::Debit => 1,
            Side::Credit => 2,
        });
        buf.extend_from_slice(&p.amount.to_le_bytes());
    }
    buf
}

fn push_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn decode(payload: &[u8]) -> Result<(String, String, Vec<Posting>), Error> {
    let mut c = Cur { b: payload, i: 0 };
    let key = c.str()?;
    let memo = c.str()?;
    if !valid_token(&key, MAX_KEY)
        || memo.len() > MAX_MEMO
        || (!memo.is_empty() && !printable(&memo))
    {
        return Err(Error::Corrupt);
    }
    let n = c.u16()? as usize;
    if n == 0 || n > MAX_POSTINGS {
        return Err(Error::Corrupt);
    }
    let mut postings = Vec::with_capacity(n);
    for _ in 0..n {
        let account = c.str()?;
        let side = match c.u8()? {
            1 => Side::Debit,
            2 => Side::Credit,
            _ => return Err(Error::Corrupt),
        };
        let amount = c.u64()?;
        if amount == 0 || !valid_token(&account, MAX_ACCOUNT) {
            return Err(Error::Corrupt);
        }
        postings.push(Posting {
            account,
            side,
            amount,
        });
    }
    if c.i != c.b.len() || check_postings(&postings).is_err() {
        return Err(Error::Corrupt);
    }
    Ok((key, memo, postings))
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.b.len() - self.i < n {
            return Err(Error::Corrupt);
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String, Error> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        let s = std::str::from_utf8(b).map_err(|_| Error::Corrupt)?;
        Ok(s.to_string())
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            let mask = (c & 1).wrapping_neg();
            c = (c >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn path(name: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("folio-{name}-{}-{n}", std::process::id()));
        p
    }

    fn xfer(amount: u64, from: &str, to: &str) -> Vec<Posting> {
        vec![
            Posting {
                account: to.into(),
                side: Side::Debit,
                amount,
            },
            Posting {
                account: from.into(),
                side: Side::Credit,
                amount,
            },
        ]
    }

    #[test]
    fn crc_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn transfer_roundtrip() {
        let p = path("round");
        let mut j = Journal::open(&p).unwrap();
        let id = j.post("k1", "rent", &xfer(1500, "cash", "rent")).unwrap();
        assert_eq!(id, 1);
        assert_eq!(j.balance("rent"), 1500);
        assert_eq!(j.balance("cash"), -1500);
        assert_eq!(j.balance("missing"), 0);
        assert_eq!(j.trial(), 0);
        assert_eq!(j.len(), 1);
        drop(j);
        let j = Journal::open(&p).unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j.txns()[0].memo, "rent");
        assert_eq!(j.balance("rent"), 1500);
        assert_eq!(j.balance("cash"), -1500);
        assert_eq!(j.trial(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unbalanced_does_not_write() {
        let p = path("unbal");
        let mut j = Journal::open(&p).unwrap();
        let before = std::fs::metadata(&p).unwrap().len();
        let err = j
            .post(
                "k",
                "",
                &[
                    Posting {
                        account: "a".into(),
                        side: Side::Debit,
                        amount: 5,
                    },
                    Posting {
                        account: "b".into(),
                        side: Side::Credit,
                        amount: 4,
                    },
                ],
            )
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Unbalanced {
                debit: 5,
                credit: 4
            }
        ));
        assert_eq!(std::fs::metadata(&p).unwrap().len(), before);
        assert!(j.is_empty());
        drop(j);
        let j = Journal::open(&p).unwrap();
        assert!(j.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn idempotent_and_conflict() {
        let p = path("idem");
        let mut j = Journal::open(&p).unwrap();
        let body = xfer(10, "cash", "fees");
        let id = j.post("same", "m", &body).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        assert_eq!(j.post("same", "m", &body).unwrap(), id);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), len);
        assert_eq!(j.len(), 1);
        let err = j.post("same", "other", &body).unwrap_err();
        assert!(matches!(err, Error::IdempotencyConflict { id: 1 }));
        assert_eq!(j.len(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rejects_bad_input() {
        let p = path("bad");
        let mut j = Journal::open(&p).unwrap();
        assert!(matches!(
            j.post("", "", &xfer(1, "a", "b")).unwrap_err(),
            Error::EmptyKey
        ));
        assert!(matches!(
            j.post("k", "", &[]).unwrap_err(),
            Error::NoPostings
        ));
        assert!(matches!(
            j.post(
                "k",
                "",
                &[Posting {
                    account: "a".into(),
                    side: Side::Debit,
                    amount: 0
                }]
            )
            .unwrap_err(),
            Error::ZeroAmount
        ));
        assert!(matches!(
            j.post("k", "", &xfer(1, " a", "b")).unwrap_err(),
            Error::BadAccount
        ));
        assert!(matches!(
            j.post("k", "", &xfer(1, "a ", "b")).unwrap_err(),
            Error::BadAccount
        ));
        assert!(matches!(
            j.post("k", "line\n", &xfer(1, "a", "b")).unwrap_err(),
            Error::BadMemo
        ));
        assert!(matches!(
            j.post(&"k".repeat(129), "", &xfer(1, "a", "b"))
                .unwrap_err(),
            Error::KeyTooLong
        ));
        assert!(j.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn split_and_same_account() {
        let p = path("split");
        let mut j = Journal::open(&p).unwrap();
        j.post(
            "s",
            "",
            &[
                Posting {
                    account: "cash".into(),
                    side: Side::Debit,
                    amount: 3,
                },
                Posting {
                    account: "cash".into(),
                    side: Side::Debit,
                    amount: 2,
                },
                Posting {
                    account: "income".into(),
                    side: Side::Credit,
                    amount: 5,
                },
            ],
        )
        .unwrap();
        assert_eq!(j.balance("cash"), 5);
        assert_eq!(j.balance("income"), -5);
        assert_eq!(j.trial(), 0);
        j.post("max", "", &xfer(u64::MAX, "a", "b")).unwrap();
        assert_eq!(j.balance("b"), u64::MAX as i128);
        assert_eq!(j.balance("a"), -(u64::MAX as i128));
        assert_eq!(j.trial(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_tail_is_dropped_bad_crc_is_not() {
        let p = path("torn");
        let mut j = Journal::open(&p).unwrap();
        j.post("k", "m", &xfer(9, "cash", "fees")).unwrap();
        drop(j);
        let good = std::fs::metadata(&p).unwrap().len();
        {
            let mut f = OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&[1, 2, 3, 4, 5, 6, 7]).unwrap();
        }
        let j = Journal::open(&p).unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j.balance("fees"), 9);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), good);
        drop(j);

        let mut bytes = std::fs::read(&p).unwrap();
        bytes[8] ^= 0xff;
        std::fs::write(&p, &bytes).unwrap();
        match Journal::open(&p) {
            Err(Error::Corrupt) => {}
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("opened a corrupt journal"),
        }
        assert_eq!(std::fs::metadata(&p).unwrap().len(), good);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn many_transfers_conserve() {
        let p = path("many");
        let mut j = Journal::open(&p).unwrap();
        let mut rng = 0x1234_5678u64;
        let mut n = 0u32;
        while n < 400 {
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            let from = rng % 20;
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            let to = rng % 20;
            if from == to {
                continue;
            }
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            let amt = (rng % 500) + 1;
            j.post(
                &format!("k{n}"),
                "",
                &xfer(amt, &format!("a{from}"), &format!("a{to}")),
            )
            .unwrap();
            n += 1;
        }
        assert_eq!(j.trial(), 0);
        assert_eq!(j.len(), 400);
        let accounts = j.accounts();
        drop(j);
        let j = Journal::open(&p).unwrap();
        assert_eq!(j.len(), 400);
        assert_eq!(j.trial(), 0);
        assert_eq!(j.accounts(), accounts);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_magic() {
        let p = path("magic");
        std::fs::write(&p, b"nope").unwrap();
        match Journal::open(&p) {
            Err(Error::Corrupt) => {}
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("opened a corrupt journal"),
        }
        let _ = std::fs::remove_file(&p);
    }
}
