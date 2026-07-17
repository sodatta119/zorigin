# Zeus — wake & power control

> **One-liner:** turn your PCs on and off from your phone. Zeus throws the bolt
> (the same bolt Zap is named for) to wake a sleeping machine.
>
> Smallest, most reliable app in the family — a great ~weekend build and a quick
> credibility win after Zap's fold-ins. Reuse Zap's **visual language + build/dist
> patterns** (see `docs/apps/README.md`); it needs almost none of Zap's transport.

## What it is

A saved list of your machines with one-tap **Wake-on-LAN**, plus **SSH-driven
shutdown / restart** and a live **up/down** indicator. For the person who leaves a
gaming/office PC at home and wants it on before they get there.

## Why it's reliable (no OS wall)

- **Wake** = send a **magic packet** (UDP broadcast to port 9, target MAC repeated
  16×). Sending a UDP broadcast is allowed on every platform (iOS needs the Local
  Network permission). No discovery, no server, nothing to break.
- **Status** = a quick ping / TCP connect to a known host/port.
- **Shutdown / restart** = SSH command to the machine. This is genuinely reliable
  and needs no special OS access.

## Features (prioritized)

1. **Saved devices** — name, MAC, broadcast address / subnet, (optional) IP + SSH.
2. **Wake** — one-tap magic packet; retry a few times.
3. **Status** — is it up? (ping / port check).
4. **Shutdown / restart over SSH** — store credentials/keys in the OS keychain
   (never plaintext). *(Security note: the app stores the user's own SSH creds
   locally; it must never prompt the user to hand credentials to anyone else.)*
5. **Home-screen widgets / quick tiles** — Wake / Shutdown without opening the app.
6. **Wake schedules** (optional, later).

## Architecture

- **Rust core**: magic-packet builder + UDP broadcast; ping/port check; an SSH
  client (a lean Rust ssh crate) for power commands. Tiny and dependency-light.
- **Front-ends**: Android (Kotlin + JNI, like the Zap app; add widget/quick-tile) +
  desktop (egui, like `zap-desktop`). WoL from desktop is trivially useful too.
- Persist the device list locally; secrets in the platform keychain/keystore.

## Platforms

All (Android + desktop primary; iOS can send broadcasts with the Local Network
permission). No cross-platform blockers of note.

## Design

Lean into the **bolt / lightning / power** theme — it ties straight to Zap's mark.
Family dark/light look, cards, amber accent. Reuse the `ZAP_SHOT`-style harness for
GUI shots.

## Non-goals

No cloud relay to wake machines over the internet (that's a different, gated
feature — keep Zeus LAN-only), no account, no agent to install on the PC beyond
standard WoL + SSH the user already has.

## First milestone

Add a device, tap Wake, machine powers on — proven on real hardware. Then add
status + SSH shutdown, then the widget.
