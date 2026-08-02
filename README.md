# moon-own

Own trading core and desktop terminal. No third-party licence, no instance limits, no
feature gates — the code is ours.

## Why it is split this way

```
        VPS (Tokyo)                              your Mac
┌────────────────────────────┐            ┌──────────────────┐
│ mooncore (headless)        │            │  terminal        │
│   binance: spot + USD-M    │◄──wire────►│   charts, book   │
│   books · resync · state   │  WS+msgpack│   order entry    │
│   API keys live ONLY here  │            │   no keys ever   │
└────────────────────────────┘            └──────────────────┘
```

API keys never leave the VPS. The terminal is a view and a command source; losing the
laptop loses nothing.

## Run it

Core (no API keys needed — market data is public):

```bash
export MOON_TOKEN=$(openssl rand -hex 24)
cargo run -p core --bin mooncore -- BTCUSDT
```

Terminal — a real window, from anywhere that can reach the core:

```bash
cd crates/terminal
MOON_TOKEN=… MOON_CORE=ws://127.0.0.1:8787 cargo run --bin moonterm -- BTCUSDT ETHUSDT
```

Or the same thing without pixels, useful for diagnosing:

```bash
MOON_TOKEN=… cargo run -p wire --example terminal -- BTCUSDT
```

Connector on its own, without a core:

```bash
cargo run -p binance --example live_feed -- BTCUSDT
```

`MOON_BIND` defaults to `127.0.0.1:8787`. Binding to a public interface is a decision to
make deliberately, alongside a firewall rule.

## Crates

| Crate | Responsibility |
|---|---|
| `domain` | Venue-agnostic types: symbols, instruments, orders, book, positions, events |
| `exchange` | The connector contract — `MarketDataSource`, `TradingVenue`, `Exchange` |
| `marketdata` | Book maintenance: snapshot attachment, gap recovery, staleness |
| `binance` | Spot and USD-M perpetuals behind one implementation |
| `wire` | The core↔terminal protocol: messages, framing, session rules |
| `storage` | Durable state: one writer, a bounded queue, an ack that means committed |
| `core` | The `mooncore` binary: connectors, read-model, WebSocket server |
| `terminal` | The `moonterm` window: order book, tape, instrument tabs |

Planned, in roadmap order: `oms`, `risk`, `bybit`, `hyperliquid`, `features`,
`reporting`, `screener`, `alerts`. No `strategy` crate — the strategy engine, backtester and
tuner are out of scope by decision of 2026-08-02; see `../moon-plan/13-roadmap.md` §1.0.

## What is not built yet

Stated plainly, because the crate table above reads like more than there is.

| Missing | Consequence today |
|---|---|
| **The entire order path** | `AccountEvent` has no producer and no `TradingVenue` is implemented. The core streams market data and nothing else — it cannot place, amend or cancel anything |
| **Priority separation in the fanout** | One `broadcast` of 8 192 carries everything. When executions start flowing they will queue behind book deltas, and the fix is much cheaper before that happens than after |
| **Persistence** | Nothing is written to disk. Restarting the core loses the tape, the candles and the read-model |
| **The message catalogue** | `wire::envelope` routes by channel and skips what it does not understand, but `ClientMsg`/`ServerMsg` still hold only the handful of messages from before the protocol work. The full command and event catalogues, idempotency and `CommandAck` arrive with roadmap tasks 1.5–1.8 |
| **`std::mem::forget` in `main.rs`** | Connectors are leaked deliberately to keep their socket tasks alive. It works, but it means the core has no clean shutdown path |

Closed since this section was written: TLS on the wire (`wss://` with rustls, and the core
refuses to start in plaintext anywhere but loopback), tests on `core/src/server.rs` (thirteen,
against a real socket), `/metrics` and `/health`, and configuration from a file.

Planning documents — reverse-engineering reports, target architecture, protocol spec and the
phased roadmap — live in `../moon-plan/`.

### Why `terminal` is a separate workspace

`core-foundation-sys` declares `links`, so one dependency graph admits exactly one version of
it — and MoonUI's macOS backend and reqwest's TLS verifier want different ones. The core ships
to a VPS and the terminal to a laptop; they share only `domain`, `wire` and `exchange`, none of
which touch reqwest. Splitting the graph costs nothing and removes the conflict at its root.

### Toolchain

Pinned to **1.97.1** in `rust-toolchain.toml`. MoonUI uses library features that landed after
1.91 (`slice_as_array` among them), so an older stable fails to compile it with a message that
points at MoonUI rather than at the toolchain.

## Decisions that are load-bearing

**Spot is the reduced case of a derivatives market.** `MarketKind` is on every identifier
from the first commit, so one connector serves both markets in one process — the thing a
MoonBot Free licence will not do at any price.

**Everything is an event-bus consumer.** The core publishes `domain::Event`; the terminal is
one subscriber, a strategy will be another. Manual and automated trading coexist without
either knowing about the other.

**Money is `rust_decimal::Decimal`, never `f64`** — exchange prices sit on decimal grids, and
binary floats round off the grid straight into a venue rejection.

## Three bugs live testing found, and what they cost

Each of these was invisible in unit tests and obvious within seconds of running.

**A snapshot never lines up with the delta stream.** A REST book snapshot is taken later than
the point the delta stream has reached, so the first deltas name a predecessor older than the
snapshot. Treating that as a gap resyncs forever: fifteen resyncs in three seconds and no
book at all. The book now has two regimes — attaching, where the overlap is discarded and it
waits for the delta that spans the snapshot, then chained, where the sequence is strict again.

**A resync loop is a rate-limit ban waiting to happen.** Every gap fetched another snapshot,
immediately. There is now a cooldown per symbol, and deltas are buffered during the fetch
instead of being thrown away and re-requested.

**A broadcast only reaches whoever is already listening.** Book snapshots are published once,
when the book is built — so a terminal connecting a minute later received deltas for a book it
did not have and sat blank forever. The core keeps a read-model and hands a session current
state on subscribe; queued deltas older than that state are discarded as stale by the same
`apply` rule.

**A book that joins the stream late was never announced.** A snapshot does not always attach
during replay — when it is ahead of the stream, the joining delta arrives from the socket
afterwards. Publishing `BookSnapshot` only from the replay path left such a book alive inside
the connector and invisible outside, so spot books never reached the terminal at all and
futures books appeared only when the timing happened to suit.

**`font-kit` is not an optional feature.** Without it the GPUI window opens, lays out
correctly, and renders no text at all — which reads as a layout bug rather than a missing
flag. One line in the manifest, twenty minutes to find.

## Known: the USD-M trade stream is silent from some networks

`btcusdt@aggTrade` on `fstream.binance.com` is accepted (`LIST_SUBSCRIPTIONS` echoes it back)
but delivers nothing, while `@depth@100ms` on the same socket streams normally and spot
`@aggTrade` works. Reproduce with:

```bash
cargo run -p binance --example raw_probe -- wss://fstream.binance.com/ws btcusdt@aggTrade
```

Books are unaffected. Worth re-testing from the Tokyo VPS.

## Third-party code

| Dependency | Licence | Use |
|---|---|---|
| [MoonUI](https://github.com/Moonbot-Tech/MoonUI) | Apache-2.0 | GPUI runtime and UI components (planned, terminal) |
| [MoonProto](https://github.com/Moonbot-Tech/MoonProto) | Apache-2.0 | Reference for protocol design |

MoonTerminal itself carries **no licence**, so no code is taken from it. Its architecture is
fair to study and reimplement; its source files are not fair to copy.

## Build

```bash
cargo test --workspace                 # 501 tests
cargo clippy --workspace --all-targets

cd crates/terminal && cargo test       # 41 more, separate workspace

./ci/check-repo-size.sh                # build output must never re-enter the index
```

The two counts above are checked by `crates/core/tests/readme.rs`, which parses this file and
compares against the number of `#[test]` attributes in the tree. A README that drifts from the
code is worse than no README, and this one had claimed 91 and 14 for several commits.
