# Formal model

Machine-checked proofs, with the [Tamarin prover](https://tamarin-prover.com),
of who can connect to a qsh session and what they can do there. The model is
symbolic (Dolev-Yao): cryptography is perfect, and the attacker controls the
network. The attacker reads, drops, changes, replays and injects all packets,
and connects to the daemon from anywhere.

The proof has two levels:

| File | Level | What it shows |
|---|---|---|
| `tls.spthy` | the connection | QUIC with TLS 1.3, modeled as signed Diffie-Hellman with the pinned daemon key. A connection of an honest client ends at the real daemon, and its key and data are secret and authentic. The exception: the daemon key was stolen *before* the connection (forward secrecy). |
| `qsh.spthy` | the application | Tokens, sessions, devices, share links, resume and revocation, on top of the connections of level 1 |

Level 2 uses what level 1 proves: an honest connection is a secure channel
(linear facts that the attacker cannot read, change or replay). The attacker
has its own connections to the daemon. After it steals the daemon key, it can
also be in the middle of honest connections that start *after* the theft
(restriction `ForwardSecrecy`).

## Results

Tamarin 1.12.0 with Maude 3.5.1. All lemmas are proven automatically, with
the default heuristic: `tls.spthy` in a few seconds, `qsh.spthy` in about
10 minutes (2 threads, less than 4 GB of memory).

`tls.spthy`:

| Lemma | Meaning |
|---|---|
| `pinned_daemon` | The client's connection ends at the daemon whose key it got over SSH. |
| `key_secret` | The connection key is secret, unless the daemon key was stolen before the connection. |
| `data_secret` | What the client sends is secret, with the same condition. |
| `data_authentic` | What the daemon receives on a client's connection comes from that client, with the same condition. |
| `exec_connection` | (sanity) A connection can complete. |

`qsh.spthy`:

| Lemma | Meaning |
|---|---|
| `bootstrap_needs_ssh` | A bootstrap token works only if an SSH login made it. |
| `bootstrap_once` | A bootstrap token works only once. |
| `bootstrap_secret` | The attacker does not learn a bootstrap token (unless it stole the daemon key). |
| `device_needs_ssh_login` | Only a connection that logged in over SSH can register a device. A stolen device token, a resumed client and a guest cannot make more tokens. |
| `device_secret` | A device token stays secret unless its file is stolen (or the daemon key). |
| `revoke_is_final` | After `qsh devices --revoke`, nothing more happens with that device: no new connection with its token, no resume of its clients, no action on its connections. |
| `owner_rooted_in_ssh` | Every owner connection comes from an SSH login: directly, or through a device token that an SSH login registered. |
| `input_authorized` | Input reaches a session only from the user, or from the holder of a stolen device token, of a read-write link *for this session*, or of the stolen daemon key. So a read-only link, or a link for another session, gives no input. |
| `typed_secret` | What the user types stays secret, also when the daemon key is stolen later (forward secrecy). |
| `exec_*` | (sanity) An SSH login, a fast start, a guest that types, and a stolen token that works until it is revoked: the model allows what qsh does, so the other lemmas are not true for an empty reason. |

## A bug that the model found

Before this model, `qsh devices --revoke` removed the device token, but not
the clients that the token had logged in. A thief who connected with a stolen
token before the revoke kept a resume token, and could come back with it for
up to 7 days. The connection that the thief had open also stayed trusted.

In the model of the old code, the resume rule (`S_Hello_Resume`) does not
look up its root in the device table. Remove `Check($D, root)` from that rule,
and Tamarin finds the attack for `revoke_is_final` (falsified, 16 steps):
steal the token, attach, revoke, resume. The fix in the daemon: each client slot keeps
the device that it logged in with (`ClientSlot.device`). A revoke removes
those slots and closes their connections (`end_device_clients`). The real
user's client then logs in again over SSH. `tests/netlab/run.py --only revoke`
checks this end to end: the thief's client ends 0.1 s after the revoke.

## Model and code

| Model | Code |
|---|---|
| `SSH_Login` | `qsh helper` over SSH asks the daemon for a bootstrap token (`LocalReq::Token`, valid 120 s) and prints it with the certificate fingerprint (`src/server/helper.rs`) |
| `tls.spthy` | rustls TLS 1.3 in quinn; the client accepts only the certificate with the pinned SHA-256 (`src/tls.rs`, `Pinned`). No 0-RTT: rustls servers accept no early data by default, and the client never calls `into_0rtt`, so the token goes only after the handshake. |
| `S_Hello_Bootstrap` | `attach()`: grant lookup in constant time, the grant is removed, `ssh_login = true` (`src/server/daemon.rs`) |
| `S_Hello_Device` | `attach()`: `Devices::check` compares SHA-256 hashes in constant time; the slot keeps the hash (`src/server/devices.rs`) |
| `S_Hello_Share` | `Grant::Share`: one session, read-only or read-write |
| `S_Hello_Resume` | `attach()` with `client_id` and the resume token; the slot keeps its role and device |
| `S_Device` | `Rpc::Device`, only if `ConnCtx.ssh_login` |
| `Revoke_Device`, `DeviceTable` | `LocalReq::Devices { revoke }`, then `end_device_clients` |
| `Compromise_Device` | a copy of `~/.qsh/known/*.json` of a client machine |

## Not in the model

- Bugs below the model: in the Rust code, in quinn or in rustls. The model
  checks the design; the tests check the code.
- Availability (denial of service).
- What the daemon can do on the client (clipboard, files, links). This is a
  policy with options (`--no-clipboard` and others), not a protocol property.
- Replay of packets inside one connection: TLS and QUIC prevent it, and the
  model has channels that the attacker cannot replay.
- Time: the bootstrap token's 120 s and the device's 30 days are not in the
  model. Expiry is like a revoke.
- What an attacker with a valid credential sees: it is like the user.

## Run

```sh
tamarin-prover --prove formal/tls.spthy
tamarin-prover --prove formal/qsh.spthy
```

To prove one lemma: `--prove=revoke_is_final`. Limit the prover's threads
and memory on a shared machine: `+RTS -N2 -M4g -RTS`.
