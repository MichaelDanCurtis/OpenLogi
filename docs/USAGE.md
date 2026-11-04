# Usage (CLI)

The `openlogi` command-line tool. For install and configuration, see the
[README](../README.md).

```sh
openlogi list                 # paired devices: slot, codename, kind, online, battery
openlogi assets sync          # pre-fetch device renders from the fastest available mirror
openlogi diag features        # dump every HID++ feature the active device reports
openlogi diag controls        # dump reprogrammable controls and capability flags
openlogi diag dpi             # read → write → read-back → restore DPI (smoke test)
openlogi diag smartshift      # toggle SmartShift and restore (smoke test)
openlogi diag lighting ff0000 # solid colour for a wired RGB keyboard (any RRGGBB hex)
openlogi host list            # each keyboard's Easy-Switch slots: current, paired, host names
openlogi host list --json     # the same, machine-readable
openlogi host switch 2        # move the keyboard (and linked mice) to slot 2
```

`openlogi host` talks to the running agent (exit status 4 when none answers).
`list` exits 3 when no keyboard matched, in both `--json` and table output.
`switch` fires the keyboard's `host_switch_actions` for that slot exactly as a
key press would, exits 0 when switched or already there, 2 when the slot is
unpaired or invalid, and 3 when no online keyboard matched. With more than one
online keyboard, pass `--device <name>`.

Running `openlogi` with no subcommand defaults to `list`. Set
`OPENLOGI_LOG=debug` for verbose tracing in the CLI, GUI, or agent.

Asset synchronization probes `assets.openlogi.org`, the versioned Cloudflare
Pages release alias, and the pinned jsDelivr npm release concurrently. The first
mirror with a valid catalog supplies every file for that synchronization run.
Set `OPENLOGI_ASSETS` or pass `openlogi assets sync --base <URL>` to use one
uniform asset origin instead of automatic mirror selection.
