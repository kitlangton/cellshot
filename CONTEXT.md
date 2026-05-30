# Context

## Glossary

### Shot

One exported visible terminal frame and any selected artifact formats derived from it. A shot can observe a launched command, piped command output, an ANSI/VT stream, or a live session.

### Frame

The versioned structured visible terminal state underlying a shot. A frame contains geometry, styled cells, and optional cursor state and can be serialized as JSON for external tooling.

### Session

A named terminal application that remains available across waiting, input, resizing, history inspection, and shots. A session is `running` while accepting input, or `exited` when its application has ended but its final shot remains inspectable until explicitly stopped. A session retains bounded normal-screen scrollback for log-oriented history inspection and raw ANSI/VT bytes for exact replay; alternate-screen TUIs are observed as shots rather than scrollback. A session may write a recording timeline while it runs; recorded sessions cannot currently be resized because recordings do not encode geometry changes.

An embedded session owns the same live terminal lifecycle in-process; the named CLI session commands are an adapter for interacting with that lifecycle across invocations.

### Recording

A timestamped terminal event timeline containing output plus client or automatic host input. A recording can be rendered to video and should be treated as potentially sensitive.

### ANSI/VT Stream

Raw terminal output bytes containing text and terminal control sequences. Files commonly use an `.ansi` suffix, but the suffix does not imply a separate container format.
