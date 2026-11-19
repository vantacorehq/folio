![folio](banner.svg)

# folio

Double-entry journal in Rust. Amounts are minor units (`u64`). A transaction is written only when the sum of the debits equals the sum of the credits.

Balances are debits minus credits (`i128`). The caller decides which accounts are assets. The sum of every account in a journal this crate wrote is 0.

## What it is not

Not a server, not a database, not multi-currency. One process writes the file. There is no lock. The currency is not stored.

Individual free of a posting does not exist. The file is append-only. To reverse something, post the opposite transaction under a new idempotency key.

## File

Magic `FOL1`, then records: little-endian length, payload, CRC-32 (ISO-HDLC). A trailing partial record is dropped on open. If a complete record has a bad checksum, `open` returns `Error::Corrupt` and does not truncate the file.

The payload is the idempotency key, the memo, and the postings (account, side, amount).

## API

```rust
let mut j = Journal::open("books.fol")?;
j.post("invoice-19", "rent", &[
    Posting { account: "rent".into(), side: Side::Debit, amount: 1500 },
    Posting { account: "cash".into(), side: Side::Credit, amount: 1500 },
])?;
assert_eq!(j.balance("cash"), -1500);
assert_eq!(j.trial(), 0);
```

The same key and the same body return the original id and write nothing. The same key with a different body is `Error::IdempotencyConflict`.

Keys and account names are printable tokens, 1 to 128 bytes, no leading or trailing space. A memo is printable and at most 512 bytes, or empty. Zero amounts are rejected.

## Build

```sh
cargo test
```

No dependencies.

## License

MIT. Copyright 2026 vantacorehq.
