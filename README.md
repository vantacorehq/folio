![folio](banner.svg)

# folio

Append-only **double-entry journal** in pure Rust.

Amounts are minor units (`u64`). A transaction is written only when debits equal credits. Balances are debits minus credits (`i128`). The sum of every account in a journal this crate wrote is always 0.

## Features

- Strict double-entry: unbalanced transactions are rejected before any write
- Idempotency keys (same key + same body → original id, no write)
- CRC-32 protected records (ISO-HDLC)
- Torn-write recovery (trailing partial record is dropped)
- Simple transfer helper
- Lookup by id and by key
- Zero dependencies

## Quick start

```rust
use folio::{Journal, Posting, Side};

let mut j = Journal::open("books.fol")?;

// Full control
j.post("invoice-19", "rent", &[
    Posting { account: "rent".into(), side: Side::Debit, amount: 1500 },
    Posting { account: "cash".into(), side: Side::Credit, amount: 1500 },
])?;

// Or the convenience transfer helper
j.transfer("pay-vendor", "materials", "cash", "expense", 420)?;

assert_eq!(j.balance("cash"), -1920);
assert_eq!(j.trial(), 0);
assert_eq!(j.account_count(), 3);

let txn = j.find_by_key("invoice-19").unwrap();
assert_eq!(txn.id, 1);
```

## API highlights

| Method | Purpose |
|--------|---------|
| `open` | Open or create journal |
| `post` | Append balanced transaction with idempotency key |
| `transfer` | Two-leg convenience transfer |
| `balance` / `accounts` / `trial` | Query balances |
| `get` / `find_by_key` | Lookup transactions |
| `sync` | Force durable write |

## Rules

- One process writes the file. There is no lock.
- Currency is not stored (caller decides the unit).
- To reverse something, post the opposite transaction under a new key.
- Zero amounts and unbalanced postings are rejected.

## Build

```sh
cargo test
```

## License

MIT
