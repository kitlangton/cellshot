# @kitlangton/terminal-control

Typed terminal application control and testing client for `termctrl driver`, with stable screen snapshots, keyboard interaction, readable logs, recordings, and opt-in failure evidence.

Install the package with Vitest after the initial npm publication:

```bash
bun add -d @kitlangton/terminal-control vitest
```

The matching native `termctrl` binary is installed automatically on macOS or GNU/Linux on arm64 or x64:

```ts
import { TerminalControl } from "@kitlangton/terminal-control"

await using terminal = await TerminalControl.make()
```

For development or custom native builds, the runtime resolves an explicit `binaryPath` first, then `TERMCTRL_BINARY`, before the installed native package.

Visible screen text and frames are stable snapshot surfaces:

```ts
await using session = await terminal.launch({ command: ["my-tui"] })
await session.screen.waitForText("Ready")
expect(await session.screen.text()).toMatchSnapshot()
```

For line-oriented output, use `session.logs.text()`; for exact ANSI/VT bytes, use `session.transcript.ansi()`. Artifact and recording configuration is opt-in because terminal output and input may contain secrets. See the repository `README.md` for the complete workflow.
