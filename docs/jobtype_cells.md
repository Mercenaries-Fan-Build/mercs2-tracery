# Jobtype `0xECAED415` — the per-cell distance test

The game posts this jobtype about 625 times per frame on the main thread; it is the single largest job
in a frame (~13 µs each, ~8.5 ms per frame in the 2026-10-24 capture). This note is the port's spec: what
the job reads, what it writes, which pieces are concurrency-safe as the game ships them, and what the
port has to do to run several copies at once.

Addresses are v1.0 (`mercs2_unpacked.exe`, sha256 `8c179a99…`). The v1.1 counterparts are listed in the
site table at the bottom.

## Dispatch

**Jobtype hash:** `0xECAED415`. **Handler:** `FUN_00735C80`.

The job is registered at build time with handler slot `_DAT_00D1CD74`; the per-tile dispatcher at
`FUN_005165C9` enqueues one instance per draw tile. The job struct the handler reads through TLS+0xC is
the standard Pimp parameter block.

## What the handler reads

The handler walks its job struct to find one entry in a per-slot table, then unpacks four values from that
entry. In the shipped build these go into globals so the inner functions can read them without taking
pointer arguments. The port passes them as arguments instead.

| Game global      | Role                                             | Port argument          |
|------------------|--------------------------------------------------|------------------------|
| `0x00F79524`     | config block for this slot (read-only)           | `Inputs::config`       |
| `0x00F7951C`     | output buffer base, indexed per cell             | `Inputs::output`       |
| `0x00F79520`     | `2.0 << (shift - 1)` as a float, from `cfg[+4]`  | `Inputs::cell_size`    |
| `0x01176260`     | viewpoint position (`float[3]`)                  | `Inputs::viewpoint`    |

The handler then iterates over the slot's cell list and calls the per-cell test for each entry.

## What the port writes

Every sub-slot visited gets a LOD classification byte (one of `0x10`, `0x20`, `0x30`, `0x40`). When the new
byte differs from the old, the change is appended to a critical-section-guarded ring at `0xDD5770 /
0xDD5790 / 0xDD5788`. That ring is shared across every thread that runs the job; the ring's own lock takes
care of concurrent appends, so the port keeps using it rather than keeping its own.

## Shared-mutable state

| Address            | Written by job       | Shared?      | Port strategy |
|--------------------|----------------------|--------------|---------------|
| `0xF79524/1C/20`   | handler (one-shot)   | yes (globals) | passed as args; port never writes them |
| `0x1176260`        | handler (one-shot)   | yes (global) | passed as arg; port never writes it |
| cell state bytes   | per-cell loop        | per cell     | one cell per thread, no contention |
| `0xDD5790` ring    | per change           | shared       | reuse the game's CS at `0xDD5770` |

Everything except the four handler-level globals is already safe for several concurrent cells (the ring is
locked, the cell state is per-cell, the config and viewpoint are read-only). Taking the globals off is the
whole point of the port.

## Site table

| v1.0        | v1.1        | Role                                            |
|-------------|-------------|-------------------------------------------------|
| `0x00735C80`| `0x00735E80`| Handler entry (`cells::run` replacement)        |
| `0x00735BD0`| TBD         | Per-cell test (inner, called from the handler)  |
| `0x00735640`| TBD         | Core per-sub-slot loop                          |
| `0x00735580`| TBD         | AABB distance helper                            |
| `0x005186F0`| TBD         | Change-list append helper                       |
| `0x00DD5770`| TBD         | Change-list `CRITICAL_SECTION`                  |

The v1.1 addresses are not yet pinned by signature; the port only needs the handler VA, which the profiler
already resolves (`JobRegister` site + slot 17 of the handler table).

## Order of operations for the port

1. **Done:** scaffold (`src/cells.rs`), interface, tests, this doc.
2. Record cases: a mode of the profiler captures the inputs the game sees + the changes it writes, one
   `.jobcase` entry per invocation.
3. Port one small, exact piece at a time (the AABB helper first), verified against recorded cases.
4. Grow the port until a chosen capture matches the game cell-for-cell.
5. Route the jobtype through workers running the port.
