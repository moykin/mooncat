# moon-own

Own trading core and desktop terminal. No third-party licence, no instance limits, no
feature gates — the code is ours.

## Why it is split this way

```
        VPS (Tokyo)                          your Mac
┌──────────────────────────┐            ┌──────────────────┐
│  core  (headless)        │            │  terminal        │
│   exchange connectors    │◄──wire────►│   GPUI + MoonUI  │
│   book · OMS · risk      │  WS+msgpack│   charts, book   │
│   strategies             │            │   order entry    │
│   API keys live ONLY here│            │   no keys ever   │
└──────────────────────────┘            └──────────────────┘
```

API keys never leave the VPS. The terminal is a view and a command source; losing the
laptop loses nothing. Same split MoonBot uses, for the same reasons.

## Crates

| Crate | Responsibility |
|---|---|
| `domain` | Venue-agnostic types: symbols, instruments, orders, book, positions, events |
| `exchange` | The connector contract — `MarketDataSource`, `TradingVenue`, `Exchange` |

Planned: `marketdata`, `oms`, `risk`, `strategy`, `storage`, `wire`, `core`, `terminal`.

## Two decisions that are load-bearing

**Spot is the reduced case of a derivatives market.** `MarketKind` is on every identifier
from the first commit, and the first connector is Binance USDT-M perpetuals — the superset,
with positions, leverage and margin mode. Spot slots in afterwards without reshaping a type.
Doing it the other way round means rewriting the account model later.

**Everything is an event-bus consumer.** The core publishes `domain::Event`; the terminal is
one subscriber, a strategy is another, the tick recorder a third. Manual and automated
trading therefore coexist without either knowing about the other, and the strategy engine
lands later without touching existing code.

Money is `rust_decimal::Decimal`, never `f64` — exchange prices sit on decimal grids, and
binary floats round off the grid straight into a venue rejection.

## Third-party code

| Dependency | Licence | Use |
|---|---|---|
| [MoonUI](https://github.com/Moonbot-Tech/MoonUI) | Apache-2.0 | GPUI runtime and UI components (planned, terminal) |
| [MoonProto](https://github.com/Moonbot-Tech/MoonProto) | Apache-2.0 | Reference for protocol design; optional MoonBot compatibility |

MoonTerminal itself carries **no licence**, so no code is taken from it. Its architecture is
fair to study and reimplement; its source files are not fair to copy.

## Build

```bash
cargo test          # 25 tests
cargo clippy --all-targets
```
