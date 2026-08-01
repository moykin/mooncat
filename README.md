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

Terminal, from anywhere that can reach it:

```bash
MOON_TOKEN=… MOON_CORE=ws://127.0.0.1:8787 cargo run -p wire --example terminal -- BTCUSDT
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
| `core` | The `mooncore` binary: connectors, read-model, WebSocket server |

Planned: `oms`, `risk`, `strategy`, `storage`, and the GPUI terminal.

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
cargo test --workspace      # 91 tests
cargo clippy --workspace --all-targets
```
