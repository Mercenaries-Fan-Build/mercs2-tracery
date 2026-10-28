# Jobtype `0xECAED415` — the per-cell sub-slot LOD test

The game posts this jobtype about 625 times per frame on the main thread; it is the single largest
leaf of the frame graph (8.4 ms of a 20261026 CrossOver frame). The profiler ASI carries a Rust port
of it (`client/src/cells.rs`) and runs that port as the jobtype's handler (`client/src/live.rs`).
This note says what the handler reads, what it writes, and how the ASI replaces it. The decode of
the arithmetic — cascade, hysteresis, fade cutoff, struct layouts — is
`docs/reverse_engineer/pimp_job_system_code_map.md` §9; the side effects a replacement must
reproduce are §9.9.

Addresses are v1.1 (`v11-patched-securom`, image `pc-v11`); §9.1 lists the v1.0 counterparts.

## Dispatch

Handler `FUN_00735E80`, slot 17 of the per-CPU handler tables. No parameters: the handler reads
the current job element through the thread's Pimp control block (`TLS[0] + 0xC`), where `+0x0C`,
`+0x10`, `+0x14` sum to the index of the slot after the cells in the element's pointer table at
`+0x20`. The cells are the pointers before that slot; the config block and the reference-point
block are the two pointers after it (§9.2).

## What the handler writes

| Target | When | Lock |
|---|---|---|
| Four globals (`0x00F78524` cfg, `0x00F7851C` reference block, `0x00F78520` cell size, `0x01175260`) | once, before the cell loop | none |
| One 16-byte entry on the change ring (`0x00DD4790`, counter `0x00DD4788`) per changed sub-slot: `entity_id`, `old \| new << 8`, 8 bytes of stack residue | as each slot changes | `CRITICAL_SECTION` `0x00DD4770`, per push |
| The slot's state byte `cell[0x80 + i]` | after the push, outside the lock | none |

The four globals are read only inside the handler's own chain; the residue bytes are copied once
(`FUN_00518890` → `0x019C50F8`) and never read; the ring has no capacity check (§9.9).

## How the ASI replaces it

`cells_shadow = off` in `mercs2_profiler.ini` (the default): the trampoline the ASI installs in the
jobtype's slot (`client/src/jobs.rs`) calls `live::game::run` instead of `FUN_00735E80`. That walks
the job element as the dispatcher does (`live::walk_tls_job`), runs `cells::job_17_run` with a
`LiveSink` that pushes each changed slot's entry under the game's critical section — the lock object
is `counter − 0x18`, from the resolved `CellsChangeRing` site — with zeros in the residue bytes, and
stores the cell's state bytes when the cell is done. It writes no globals: nothing reads them when
the chain does not run. The port takes its inputs as arguments and writes nothing shared outside the
ring, so it is re-entrant.

`cells_shadow = on`: the game's handler runs, and `client/src/shadow.rs` runs the port beside every
invocation on the same inputs into a buffer, compares that with the ring entries the handler wrote,
and logs each difference with the cell, the slot and the slot's arithmetic. This is the regression
check for the port; it pays for both handlers. On CrossOver run it with `sample_rate_hz = 1`
(memory `crossover-sampler-corrupts-x87`).

## Site table

| Site | What the ASI takes from it |
|---|---|
| `JobRegister` (`0x0084B230`) | the jobtype count and the two handler tables; slot 17 is wrapped like every other |
| `CellsChangeRing` (`0x00735AEF`) | the counter and ring base; the lock object is `counter − 0x18` (`CellsChangeRing::critical_section`) |

Both resolve by byte pattern (`mercs2_formats::profiler_sites`), so the ASI needs no per-build
addresses.
