# Ablation study

Each mechanism of qsh is there for a reason. This study turns them off one
at a time and measures what each one gives, in the network lab
(`tests/netlab`). It also measured new mechanisms before they went in, and it
removed one that gave nothing.

## Method

**Lab.** `netem.py` emulates the network between two TUN devices on one Linux
machine: delay and jitter, loss, a rate limit with a queue, outages and
blocked UDP. Profiles (one way; the round trip is twice the delay):

| Profile | Delay | Loss | Other |
|---|---|---|---|
| fiber | 10 ms | – | – |
| lte | 40±20 ms | 3 %, bursts of 3 packets (mean) | – |
| bad | 80±40 ms | 10 %, bursts of 4 packets | 2 % reordering |
| awful | 150±60 ms | 25 %, bursts of 5 packets | – |
| lte-t, bad-t, awful-t | as lte, bad, awful | the same share, in fades of 60, 150, 300 ms (mean) | – |
| lossy | 40±10 ms | 5 %, independent | – |
| slow | 30 ms | – | 1 Mbit/s, 2 s queue |
| slow-shallow | 30 ms | – | 1 Mbit/s, 0.2 s queue |
| dsl | 20 ms | – | 10 Mbit/s, 0.5 s queue |

In the first loss model a burst has a length in packets. For a sparse flow
(pings and keys, four packets a second) a burst of 5 packets is more than a
second of silence, and each packet that a sender adds makes the burst end
sooner. That is not how a radio fade works, so the `-t` profiles have loss in
time: in a fade, all packets are lost, however many there are. Results that
differ between the two models show where the model matters.

**Variants.** `QSH_ABLATE=name` turns one mechanism off, in the client and in
the lab daemon (`src/ablate.rs`); `name=value` sets a value for a sweep. "base"
is the build as it is. `nofast` is the client option `--no-fast` (always log
in over SSH).

**Experiments** (`tests/netlab/ablate.py`):

| Experiment | Measures |
|---|---|
| startup | time to the prompt of a new session, host known (fast start) |
| echo | time until a typed key shows, without local echo; pauses of 0.3–3 s between keys |
| ctrlc | Ctrl-C after a flood of `yes`: time until the next command runs (slow-fresh: 0.5 s after the start of the connection) |
| flow | output rate of `seq` (3.4 MB; 0.6 MB on slow-shallow) |
| idle | "no reply" badges per minute, 45 s without input |
| outage | no network for 8 or 30 s during a counter: time until output continues after the network returns; every line once, in order |
| udpblock | UDP blocked during a counter: time until output continues on the SSH tunnel; every line once, in order |
| integrity | 60,000 numbered lines: time, and every line once, in order |

**Order.** The driver runs rounds. In each round the variants come in random
order, and each variant runs all its experiments (in random order). So a slow
change of the machine (other load, temperature) affects all variants alike.
The daemon starts again for each variant. Each client uses a new session name.

**Statistics.** For each experiment and variant: the median with a 95 %
bootstrap interval, and p90. Against base: a Mann–Whitney test (two-sided,
normal approximation), and for the tail, the share of samples above twice the
base median (failures count as slow), with Fisher's exact test. Before the
confirmatory run, each mechanism got one **primary metric**, chosen from the
pilot run and the A/B runs; the Holm correction is over these. All other rows
are exploratory: with about 100 rows, some rows show a "difference" by chance.

Samples of one client are not independent (10 keys in one echo run share the
link's state). So the echo p values are too small; the tail counts are the
better guide there.

**Steps.**

1. Pilot: all mechanisms, the build before this study (4 rounds). It showed
   the size of the effects and the noise, and it found faults in the harness
   (below).
2. A/B runs for new mechanisms and a sweep of the start window.
3. Confirmatory run: the final build, split over four labs at the same time,
   6–10 rounds.

## Results

### Primary metrics

Confirmatory run on the final build (medians; n = 8 for Ctrl-C and flow, 6
for idle and outage, 20 for startup, 90 key echoes). "Off" is the build
without the mechanism. Holm correction over these 13 tests:

| Mechanism | Metric | With | Off | p | Holm |
|---|---|---|---|---|---|
| Skip to the screen after Ctrl-C | Ctrl-C in a flood, slow | 0.42 s | 8.81 s | 0.0008 | yes |
| Start window of 32 packets (off: 240 KB) | Ctrl-C 0.5 s after the start, slow | 0.45 s | 4.01 s | 0.0008 | yes |
| BBR (off: Cubic) | output rate, lte | 1.88 MB/s | 0.15 MB/s | 0.0008 | yes |
| Four pings a second (off: one) | idle "no reply" badges, awful-t | 0 a minute | 6.7 a minute | 0.003 | yes |
| Reconnect with handshakes every 0.5 s (off: the old race) | output after a 30 s outage | 0.28 s | 1.65 s | 0.004 | yes |
| Fast start (off: `--no-fast`) | time to the prompt, bad | 0.65 s | 3.88 s (1 failed) | 0.000003 | yes |
| Output window minimum 32 KB (off: 256 KB) | Ctrl-C in a flood, slow | 0.42 s | 2.22 s | 0.012 | no (see below) |
| Window floor of 16 packets | time to the prompt, awful | 2.61 s | 1.60 s | 0.39 | no |
| Known RTT for the handshake | time to the prompt, awful-t | 2.28 s | 1.03 s | 0.10 | no |
| Retry of the fast start | time to the prompt, awful-t | 2.28 s | 1.88 s | 0.85 | no |
| Copies of keys and echoes | key echo, awful | 0.40 s | 0.52 s | 0.27 | no (see below) |
| Parallel handshakes | time to the prompt, awful | 2.61 s | 2.11 s | 0.73 | no |
| SSH helper in advance | UDP blocked: output on the tunnel | 4.94 s | 5.00 s | 0.52 | no (see below) |

### Congestion control and windows

These effects are large and clear in all runs (pilot, confirmatory run and
sweeps).

- **BBR.** With Cubic, 3.4 MB on lte moved at 0.15 MB/s instead of 1.88, and
  60,000 lines took 21 s instead of 2.3 s on bad, 76 s instead of 3.2 s on
  awful. A Ctrl-C in a flood on the slow link took 6.1 s instead of 0.42 s:
  Cubic fills the 2 s queue.
- **Skip to the screen after Ctrl-C.** Without it, a Ctrl-C in a flood on a
  1 Mbit/s link took 8.8 s with a 2 s queue and also with a 0.2 s queue: the
  client must get all output that the program wrote before it stopped.
- **Output window.** With a minimum of 256 KB instead of 32 KB, a Ctrl-C on a
  1 Mbit/s link took 2.2–2.3 s instead of 0.42–0.45 s, in all three slow
  profiles (8 of 8 against 0 of 8 above twice the base median in two of
  them). In the primary test, one base sample of 4.9 s made p = 0.012 (see
  [Open](#open)).
- **Start window.** A sweep chose the value (n = 9–10 each):

  | Start window | fiber, 3.4 MB | lte, 3.4 MB | Ctrl-C 0.5 s after the start, slow | Ctrl-C in a flood, slow |
  |---|---|---|---|---|
  | 10 packets (RFC 9002; the old value) | 6.25 MB/s | 1.81 MB/s | 0.40 s | 0.36 s |
  | **32 packets (now)** | **8.60 MB/s** | 1.88 MB/s | 0.44 s | 0.41 s |
  | 64 packets | 8.95 MB/s | 2.04 MB/s | 1.04 s | 0.42 s |
  | 128 packets | 9.20 MB/s | 2.35 MB/s | 2.50 s | 0.52 s |
  | 240 KB (quinn) | 8.85 MB/s | 2.63 MB/s | 4.05 s | 1.56 s |

  32 packets gave 38 % more on fiber (p = 0.002) at no significant cost. A
  minimum output window of 16 KB gave nothing; 64 KB made Ctrl-C 0.18–0.28 s
  slower.
- **Window floor of 16 packets.** It came in for a start on awful that
  stalled for 120 s (a window of 4 packets blocked by lost pings). In the
  confirmatory run it made no significant difference to the time to the
  prompt (the startup times on awful spread from 0.7 s to 10 s), and without
  it a Ctrl-C right after the start took 0.63 s instead of 0.45 s. It stays:
  it costs nothing measurable, and it removes a stall that a window of 4
  packets can cause.

### Keys and echoes

- **Copies.** In the A/B run (n = 60–70 keys per profile), copies cut the
  slow tail of the echo: echoes slower than twice the median, awful 11 of 60
  with copies against 27 of 60 without (p = 0.003), awful-t 14 of 60 against
  31 of 70 (p = 0.016); echo p90 on bad 0.24 s against 1.05 s. In the
  confirmatory run the direction was the same in all lossy profiles (p90
  without copies: awful 2.58 s against 1.15 s, awful-t 2.40 s against
  1.64 s, bad 0.80 s against 0.37 s, bad-t 0.68 s against 0.44 s), but the
  tail counts were weaker (awful 30 of 90 against 19 of 90, p = 0.09). The
  primary test compares medians, and copies do not change the median. The
  metric was a bad choice for this mechanism: it acts on the tail. The keys
  of one run share the state of the link, so these p values are too small
  in both runs.
- **Four pings a second.** With one ping a second, an idle session on awful-t
  showed 6.7 false "no reply" badges a minute instead of 0. A lost ping does
  not go again, so fewer pings make longer gaps.

### Start

- **Fast start.** Without it (a login over SSH each time), the prompt came
  after 0.51 s instead of 0.14 s on fiber, 3.9 s instead of 0.65 s on bad,
  and on awful 10 of 20 starts over SSH failed within 60 s.
- **Known RTT, retry, parallel handshakes, window floor.** On awful and
  awful-t, none of these changed the time to the prompt significantly with
  n = 20. One start takes 0.7–10 s there, and the spread of the base
  (95 % interval of the median: 0.83–5.3 s) covers all variants. A study of
  the phases of 85 starts per variant (bad, awful, awful-t) showed one clear
  effect: the parallel handshakes cut the slow tail of the QUIC handshake
  (mean 0.53 s against 0.91 s, p90 1.0 s against 1.7 s). The time after the
  handshake (the first request and its answer) did not change with any
  variant.
- **Removed: pings during the first exchange.** The idea: after the QUIC
  handshake, pings every quarter round trip let QUIC find a lost packet of
  the first request without the doubled probe timeout. It gave no
  measurable gain (time after the handshake: mean 0.96 s with the pings,
  0.88 s without; p90 2.75 s and 1.72 s), so it is not in the build.

### Network changes

- **Reconnect.** With a fresh handshake every 0.5 s (up to six at a time),
  output continued 0.28 s after a 30 s outage; with the old race (one
  attempt, then a wait), after 1.65 s.
- **SSH helper in advance.** In three A/B runs, output continued on the SSH
  tunnel after 3.32–3.40 s with the helper in advance, 4.87–5.07 s without
  (p ≤ 0.01 each). In the confirmatory run there was no difference: with
  five labs on one machine, the SSH login took longer, the helper was not
  ready when the race started at 3 s, and the race then started a second
  SSH login. The race now waits for a helper that is starting. A follow-up
  run under the same load (four labs, n = 8): 3.41 s with the helper in
  advance, 5.09 s without (p = 0.005).
- **Cost of the helper in advance.** The first version started the helper
  after 1.5 s without an echo. On lossy links that was 26 SSH logins in
  15 minutes of idle sessions, and two moves to the TCP tunnel, which is a
  bad path on a lossy link. Now the path must be silent (no packet at all
  for 1.5 s), and the wait between two helpers doubles from 1 to 16 minutes
  until a move to the tunnel.

### Open

- **A slow Ctrl-C now and then.** On the slow link (1 Mbit/s, 2 s queue),
  about 3 of 31 Ctrl-Cs took 2–4.9 s (the others 0.40–0.47 s). The daemon
  logs each interrupt; in the slow case, BBR's congestion window was 135 KB
  (normal: 19 KB), about nine times the rate of the link. The output window
  (two congestion windows) then let 555 KB go to QUIC, the round trip rose
  to 1.4 s, and the skip after Ctrl-C cannot take back output that QUIC
  already has: 555 KB at 125 KB/s is 4.4 s. A guard that sets the output
  window to its minimum when the round trip is more than twice its minimum
  plus 200 ms had no measurable cost on fiber, dsl and lte, but in 40 runs
  (20 with, 20 without) the slow case did not come, so its gain is not shown
  and it is not in the build. Not measured: its effect on the SSH tunnel,
  where TCP buffers also raise the round trip.
- **Startup on awful.** A start can take 10 s on the awful profiles. In one
  case the client was attached after 0.75 s, but no output came on that
  connection for 10 s, until a move to a new connection. A second run with
  the fixed harness (n = 20 per variant) again showed no significant effect
  of the known RTT, the retry, the parallel handshakes or the window floor on
  awful and awful-t (base medians 2.0 s and 1.8 s, p90 8.2 s and 7.0 s).

### Full table

All rows of the confirmatory run (`ablate.py --summary`):


| Experiment | Variant | n | Fails | Median | 95% CI | p90 | Change | p | > 2× base median |
|---|---|---|---|---|---|---|---|---|---|
| startup/fiber | base | 20 | 0 | 139 ms | 136 ms–149 ms | 186 ms |  |  |  |
| startup/fiber | knownrtt | 20 | 0 | 138 ms | 136 ms–146 ms | 159 ms | -1% | 0.79 | 0/20 vs 0/20 (p 1) |
| startup/fiber | nofast | 20 | 0 | 508 ms | 503 ms–511 ms | 572 ms | +264% | 6e-08 * | 20/20 vs 0/20 (p 1.5e-11) |
| startup/bad | base | 20 | 0 | 648 ms | 502 ms–894 ms | 3.34 s |  |  |  |
| startup/bad | bbr | 20 | 0 | 559 ms | 499 ms–885 ms | 1.61 s | -14% | 0.73 | 5/20 vs 4/20 (p 1) |
| startup/bad | fastrace | 20 | 0 | 792 ms | 510 ms–1.66 s | 2.98 s | +22% | 0.61 | 8/20 vs 4/20 (p 0.3) |
| startup/bad | fastretry | 20 | 0 | 576 ms | 470 ms–1.03 s | 1.62 s | -11% | 0.70 | 4/20 vs 4/20 (p 1) |
| startup/bad | floor | 20 | 0 | 745 ms | 532 ms–1.09 s | 1.59 s | +15% | 0.70 | 3/20 vs 4/20 (p 1) |
| startup/bad | initwin | 20 | 0 | 486 ms | 441 ms–673 ms | 1.94 s | -25% | 0.10 | 4/20 vs 4/20 (p 1) |
| startup/bad | knownrtt | 20 | 0 | 721 ms | 490 ms–1.00 s | 1.78 s | +11% | 0.94 | 3/20 vs 4/20 (p 1) |
| startup/bad | nofast | 19 | 1 | 3.88 s | 3.11 s–8.27 s | 10.34 s | +499% | 5e-06 * | 20/20 vs 4/20 (p 1.5e-07) |
| startup/awful | base | 19 | 1 | 2.61 s | 829 ms–5.26 s | 7.45 s |  |  |  |
| startup/awful | bbr | 20 | 0 | 1.98 s | 1.27 s–3.24 s | 4.51 s | -24% | 0.78 | 2/20 vs 7/20 (p 0.13) |
| startup/awful | fastrace | 20 | 0 | 2.11 s | 1.35 s–3.97 s | 6.20 s | -19% | 0.93 | 4/20 vs 7/20 (p 0.48) |
| startup/awful | fastretry | 20 | 0 | 1.90 s | 1.40 s–2.61 s | 4.44 s | -27% | 0.57 | 1/20 vs 7/20 (p 0.044) |
| startup/awful | floor | 20 | 0 | 1.60 s | 1.09 s–2.42 s | 5.79 s | -39% | 0.54 | 2/20 vs 7/20 (p 0.13) |
| startup/awful | initwin | 20 | 0 | 1.71 s | 1.16 s–4.00 s | 7.91 s | -34% | 0.96 | 5/20 vs 7/20 (p 0.73) |
| startup/awful | knownrtt | 20 | 0 | 1.51 s | 1.14 s–3.25 s | 4.87 s | -42% | 0.48 | 1/20 vs 7/20 (p 0.044) |
| startup/awful | nofast | 10 | 10 | 11.79 s | 6.86 s–17.53 s | 20.24 s | +352% | 0.0003 * | 19/20 vs 7/20 (p 0.00014) |
| startup/awful-t | base | 20 | 0 | 2.28 s | 895 ms–3.89 s | 6.89 s |  |  |  |
| startup/awful-t | bbr | 20 | 0 | 1.61 s | 1.20 s–2.75 s | 7.30 s | -29% | 0.63 | 4/20 vs 5/20 (p 1) |
| startup/awful-t | fastrace | 20 | 0 | 1.54 s | 935 ms–3.31 s | 5.07 s | -32% | 0.48 | 5/20 vs 5/20 (p 1) |
| startup/awful-t | fastretry | 20 | 0 | 1.88 s | 1.25 s–3.43 s | 4.20 s | -18% | 0.85 | 2/20 vs 5/20 (p 0.41) |
| startup/awful-t | floor | 20 | 0 | 1.34 s | 818 ms–1.88 s | 3.08 s | -41% | 0.12 | 1/20 vs 5/20 (p 0.18) |
| startup/awful-t | initwin | 20 | 0 | 2.77 s | 1.33 s–3.97 s | 6.10 s | +22% | 0.94 | 3/20 vs 5/20 (p 0.69) |
| startup/awful-t | knownrtt | 20 | 0 | 1.03 s | 803 ms–2.01 s | 3.63 s | -55% | 0.10 | 2/20 vs 5/20 (p 0.41) |
| startup/awful-t | nofast | 19 | 1 | 17.76 s | 14.51 s–19.26 s | 27.48 s | +681% | 1e-07 * | 20/20 vs 5/20 (p 7.7e-07) |
| echo/lte | base | 90 | 0 | 86 ms | 79 ms–93 ms | 111 ms |  |  |  |
| echo/lte | bbr | 90 | 0 | 87 ms | 84 ms–88 ms | 109 ms | +1% | 0.63 | 0/90 vs 3/90 (p 0.25) |
| echo/lte | copies | 90 | 0 | 82 ms | 80 ms–88 ms | 105 ms | -5% | 0.59 | 2/90 vs 3/90 (p 1) |
| echo/lte | floor | 90 | 0 | 85 ms | 81 ms–92 ms | 112 ms | -0% | 0.94 | 3/90 vs 3/90 (p 1) |
| echo/lte | ping4 | 80 | 0 | 82 ms | 78 ms–86 ms | 108 ms | -4% | 0.33 | 2/80 vs 3/90 (p 1) |
| echo/bad | base | 90 | 0 | 182 ms | 170 ms–203 ms | 367 ms |  |  |  |
| echo/bad | bbr | 90 | 0 | 182 ms | 172 ms–196 ms | 257 ms | -0% | 0.53 | 5/90 vs 10/90 (p 0.28) |
| echo/bad | copies | 90 | 0 | 170 ms | 162 ms–189 ms | 800 ms | -7% | 0.65 | 15/90 vs 10/90 (p 0.39) |
| echo/bad | floor | 90 | 0 | 184 ms | 173 ms–196 ms | 303 ms | +1% | 0.58 | 7/90 vs 10/90 (p 0.61) |
| echo/bad | ping4 | 80 | 0 | 160 ms | 149 ms–178 ms | 237 ms | -12% | 0.002 | 6/80 vs 10/90 (p 0.45) |
| echo/awful | base | 90 | 0 | 404 ms | 383 ms–487 ms | 1.15 s |  |  |  |
| echo/awful | bbr | 90 | 0 | 380 ms | 346 ms–427 ms | 1.34 s | -6% | 0.25 | 17/90 vs 19/90 (p 0.85) |
| echo/awful | copies | 90 | 0 | 520 ms | 358 ms–709 ms | 2.58 s | +29% | 0.27 | 30/90 vs 19/90 (p 0.093) |
| echo/awful | floor | 90 | 0 | 392 ms | 369 ms–421 ms | 1.30 s | -3% | 0.59 | 17/90 vs 19/90 (p 0.85) |
| echo/awful | ping4 | 90 | 0 | 371 ms | 350 ms–402 ms | 1.67 s | -8% | 0.20 | 17/90 vs 19/90 (p 0.85) |
| echo/awful-t | base | 90 | 0 | 382 ms | 357 ms–468 ms | 1.64 s |  |  |  |
| echo/awful-t | bbr | 90 | 0 | 360 ms | 333 ms–407 ms | 938 ms | -6% | 0.07 | 19/90 vs 27/90 (p 0.23) |
| echo/awful-t | copies | 90 | 0 | 612 ms | 371 ms–846 ms | 2.40 s | +60% | 0.13 | 38/90 vs 27/90 (p 0.12) |
| echo/awful-t | floor | 90 | 0 | 374 ms | 349 ms–405 ms | 1.78 s | -2% | 0.44 | 24/90 vs 27/90 (p 0.74) |
| echo/awful-t | ping4 | 80 | 0 | 381 ms | 343 ms–523 ms | 3.03 s | -0% | 0.81 | 26/80 vs 27/90 (p 0.74) |
| echo/lossy | base | 90 | 0 | 86 ms | 84 ms–89 ms | 96 ms |  |  |  |
| echo/lossy | copies | 90 | 0 | 81 ms | 80 ms–84 ms | 95 ms | -5% | 0.05 | 6/90 vs 1/90 (p 0.12) |
| echo/lte-t | base | 90 | 0 | 88 ms | 83 ms–92 ms | 110 ms |  |  |  |
| echo/lte-t | copies | 90 | 0 | 84 ms | 79 ms–88 ms | 103 ms | -5% | 0.11 | 4/90 vs 3/90 (p 1) |
| echo/bad-t | base | 90 | 0 | 185 ms | 176 ms–203 ms | 439 ms |  |  |  |
| echo/bad-t | copies | 90 | 0 | 173 ms | 164 ms–184 ms | 675 ms | -7% | 0.25 | 18/90 vs 11/90 (p 0.22) |
| ctrlc/slow-fresh | base | 8 | 0 | 446 ms | 410 ms–468 ms | 475 ms |  |  |  |
| ctrlc/slow-fresh | bbr | 8 | 0 | 853 ms | 396 ms–861 ms | 874 ms | +91% | 0.06 | 1/8 vs 0/8 (p 1) |
| ctrlc/slow-fresh | floor | 8 | 0 | 632 ms | 559 ms–649 ms | 710 ms | +42% | 0.0008 | 0/8 vs 0/8 (p 1) |
| ctrlc/slow-fresh | initwin | 8 | 0 | 4.01 s | 3.87 s–4.03 s | 4.06 s | +800% | 0.0008 | 8/8 vs 0/8 (p 0.00016) |
| ctrlc/slow-fresh | skip | 8 | 0 | 8.80 s | 8.56 s–8.85 s | 8.85 s | +1875% | 0.0008 | 8/8 vs 0/8 (p 0.00016) |
| ctrlc/slow-fresh | win32k | 8 | 0 | 2.29 s | 2.21 s–2.34 s | 2.34 s | +414% | 0.0008 | 8/8 vs 0/8 (p 0.00016) |
| ctrlc/slow | base | 8 | 0 | 421 ms | 407 ms–433 ms | 1.77 s |  |  |  |
| ctrlc/slow | bbr | 8 | 0 | 6.10 s | 5.52 s–6.23 s | 6.31 s | +1348% | 0.01 | 7/8 vs 1/8 (p 0.01) |
| ctrlc/slow | floor | 8 | 0 | 434 ms | 321 ms–638 ms | 731 ms | +3% | 1.00 | 0/8 vs 1/8 (p 1) |
| ctrlc/slow | initwin | 8 | 0 | 3.01 s | 1.50 s–6.55 s | 6.61 s | +615% | 0.005 | 8/8 vs 1/8 (p 0.0014) |
| ctrlc/slow | skip | 8 | 0 | 8.81 s | 8.79 s–8.83 s | 8.84 s | +1991% | 0.0008 | 8/8 vs 1/8 (p 0.0014) |
| ctrlc/slow | win32k | 8 | 0 | 2.22 s | 2.21 s–2.25 s | 2.25 s | +428% | 0.01 | 8/8 vs 1/8 (p 0.0014) |
| ctrlc/slow-shallow | base | 8 | 0 | 439 ms | 400 ms–466 ms | 466 ms |  |  |  |
| ctrlc/slow-shallow | bbr | 8 | 0 | 518 ms | 497 ms–541 ms | 548 ms | +18% | 0.009 | 0/8 vs 0/8 (p 1) |
| ctrlc/slow-shallow | floor | 8 | 0 | 532 ms | 340 ms–551 ms | 560 ms | +21% | 0.21 | 0/8 vs 0/8 (p 1) |
| ctrlc/slow-shallow | initwin | 8 | 0 | 1.61 s | 1.43 s–1.79 s | 1.79 s | +267% | 0.0008 | 8/8 vs 0/8 (p 0.00016) |
| ctrlc/slow-shallow | skip | 8 | 0 | 8.82 s | 8.77 s–8.87 s | 8.88 s | +1911% | 0.0008 | 8/8 vs 0/8 (p 0.00016) |
| ctrlc/slow-shallow | win32k | 8 | 0 | 2.27 s | 1.87 s–2.31 s | 2.32 s | +418% | 0.0008 | 8/8 vs 0/8 (p 0.00016) |
| ctrlc/lte | base | 8 | 0 | 220 ms | 148 ms–242 ms | 257 ms |  |  |  |
| ctrlc/lte | bbr | 8 | 0 | 326 ms | 294 ms–395 ms | 438 ms | +48% | 0.002 | 1/8 vs 0/8 (p 1) |
| ctrlc/lte | floor | 8 | 0 | 219 ms | 162 ms–257 ms | 324 ms | -0% | 0.75 | 1/8 vs 0/8 (p 1) |
| ctrlc/lte | initwin | 8 | 0 | 204 ms | 155 ms–263 ms | 286 ms | -7% | 0.92 | 0/8 vs 0/8 (p 1) |
| ctrlc/lte | skip | 8 | 0 | 241 ms | 211 ms–271 ms | 281 ms | +10% | 0.21 | 0/8 vs 0/8 (p 1) |
| ctrlc/lte | win32k | 8 | 0 | 227 ms | 203 ms–282 ms | 296 ms | +3% | 0.46 | 0/8 vs 0/8 (p 1) |
| flow/fiber | base | 8 | 0 | 7.85 MB/s | 7.15 MB/s–8.90 MB/s | 10.10 MB/s |  |  |  |
| flow/fiber | bbr | 8 | 0 | 8.60 MB/s | 7.32 MB/s–9.04 MB/s | 9.15 MB/s | +10% | 0.34 |  |
| flow/fiber | floor | 8 | 0 | 8.06 MB/s | 5.94 MB/s–10.23 MB/s | 10.65 MB/s | +3% | 0.75 |  |
| flow/fiber | initwin | 8 | 0 | 9.40 MB/s | 8.50 MB/s–11.65 MB/s | 12.59 MB/s | +20% | 0.04 |  |
| flow/fiber | win32k | 8 | 0 | 8.97 MB/s | 8.10 MB/s–10.31 MB/s | 10.40 MB/s | +14% | 0.09 |  |
| flow/dsl | base | 8 | 0 | 1.15 MB/s | 1.15 MB/s–1.15 MB/s | 1.15 MB/s |  |  |  |
| flow/dsl | bbr | 8 | 0 | 1.12 MB/s | 1.11 MB/s–1.13 MB/s | 1.13 MB/s | -3% | 0.0008 |  |
| flow/dsl | floor | 8 | 0 | 1.13 MB/s | 1.11 MB/s–1.15 MB/s | 1.15 MB/s | -2% | 0.009 |  |
| flow/dsl | initwin | 8 | 0 | 1.15 MB/s | 1.14 MB/s–1.15 MB/s | 1.15 MB/s | -0% | 0.17 |  |
| flow/dsl | win32k | 8 | 0 | 1.12 MB/s | 1.12 MB/s–1.14 MB/s | 1.14 MB/s | -2% | 0.0008 |  |
| flow/lte | base | 8 | 0 | 1.88 MB/s | 1.52 MB/s–2.25 MB/s | 2.26 MB/s |  |  |  |
| flow/lte | bbr | 8 | 0 | 0.15 MB/s | 0.12 MB/s–0.18 MB/s | 0.19 MB/s | -92% | 0.0008 |  |
| flow/lte | floor | 8 | 0 | 2.01 MB/s | 1.74 MB/s–2.20 MB/s | 2.24 MB/s | +7% | 0.60 |  |
| flow/lte | initwin | 8 | 0 | 2.43 MB/s | 2.14 MB/s–2.50 MB/s | 2.53 MB/s | +29% | 0.02 |  |
| flow/lte | win32k | 8 | 0 | 2.04 MB/s | 1.74 MB/s–2.15 MB/s | 2.18 MB/s | +8% | 0.67 |  |
| flow/slow-shallow | base | 8 | 0 | 0.12 MB/s | 0.12 MB/s–0.12 MB/s | 0.12 MB/s |  |  |  |
| flow/slow-shallow | bbr | 8 | 0 | 0.12 MB/s | 0.12 MB/s–0.12 MB/s | 0.12 MB/s | +1% | 0.006 |  |
| flow/slow-shallow | floor | 8 | 0 | 0.11 MB/s | 0.11 MB/s–0.11 MB/s | 0.11 MB/s | -9% | 0.0008 |  |
| flow/slow-shallow | initwin | 8 | 0 | 0.11 MB/s | 0.11 MB/s–0.11 MB/s | 0.11 MB/s | -6% | 0.0008 |  |
| flow/slow-shallow | win32k | 8 | 0 | 0.12 MB/s | 0.11 MB/s–0.12 MB/s | 0.12 MB/s | -1% | 0.05 |  |
| idle/awful | base | 6 | 0 | 3.33/min | 2.00/min–6.67/min | 6.67/min |  |  |  |
| idle/awful | bbr | 6 | 0 | 1.33/min | 0.67/min–2.67/min | 2.67/min | -60% | 0.04 | 0/6 vs 1/6 (p 1) |
| idle/awful | floor | 6 | 0 | 3.33/min | 1.33/min–5.33/min | 5.33/min | +0% | 0.62 | 1/6 vs 1/6 (p 1) |
| idle/awful | ping4 | 6 | 0 | 4.00/min | 3.33/min–4.67/min | 4.67/min | +20% | 0.68 | 0/6 vs 1/6 (p 1) |
| idle/awful-t | base | 6 | 0 | 0.00/min | 0.00/min–0.67/min | 0.67/min |  |  |  |
| idle/awful-t | bbr | 6 | 0 | 1.33/min | 0.00/min–1.33/min | 1.33/min |  | 0.09 | 4/6 vs 1/6 (p 0.24) |
| idle/awful-t | floor | 6 | 0 | 0.67/min | 0.00/min–1.33/min | 1.33/min |  | 0.24 | 3/6 vs 1/6 (p 0.55) |
| idle/awful-t | ping4 | 6 | 0 | 6.67/min | 4.00/min–8.67/min | 8.67/min |  | 0.003 | 6/6 vs 1/6 (p 0.015) |
| outage/8s | base | 6 | 0 | 129 ms | 111 ms–155 ms | 155 ms |  |  |  |
| outage/8s | ping4 | 6 | 0 | 111 ms | 81 ms–134 ms | 134 ms | -14% | 0.26 | 0/6 vs 0/6 (p 1) |
| outage/8s | reconnect | 6 | 0 | 164 ms | 112 ms–270 ms | 270 ms | +27% | 0.11 | 2/6 vs 0/6 (p 0.45) |
| outage/30s | base | 6 | 0 | 283 ms | 170 ms–430 ms | 430 ms |  |  |  |
| outage/30s | ping4 | 6 | 0 | 289 ms | 211 ms–1.82 s | 1.82 s | +2% | 0.75 | 1/6 vs 0/6 (p 1) |
| outage/30s | reconnect | 6 | 0 | 1.65 s | 1.61 s–1.72 s | 1.72 s | +483% | 0.004 | 6/6 vs 0/6 (p 0.0022) |
| udpblock/lte | base | 6 | 0 | 4.94 s | 3.31 s–5.45 s | 5.45 s |  |  |  |
| udpblock/lte | prewarm | 6 | 0 | 5.00 s | 4.89 s–5.52 s | 5.52 s | +1% | 0.52 | 0/6 vs 0/6 (p 1) |
| integrity/bad | base | 8 | 1 | 2.25 s | 2.03 s–2.64 s | 3.95 s |  |  |  |
| integrity/bad | bbr | 9 | 0 | 21.14 s | 16.16 s–25.79 s | 25.96 s | +841% | 0.0005 * | 9/9 vs 2/9 (p 0.0023) |
| integrity/bad | copies | 9 | 0 | 2.71 s | 2.08 s–3.56 s | 3.70 s | +21% | 0.34 | 0/9 vs 2/9 (p 0.47) |
| integrity/bad | floor | 9 | 0 | 2.32 s | 1.90 s–2.90 s | 3.61 s | +3% | 0.85 | 1/9 vs 2/9 (p 1) |
| integrity/awful | base | 8 | 1 | 3.22 s | 2.67 s–5.74 s | 5.96 s |  |  |  |
| integrity/awful | bbr | 9 | 0 | 75.52 s | 44.66 s–111.93 s | 114.26 s | +2243% | 0.0005 * | 9/9 vs 2/9 (p 0.0023) |
| integrity/awful | copies | 9 | 0 | 4.13 s | 3.15 s–4.97 s | 5.03 s | +28% | 0.34 | 0/9 vs 2/9 (p 0.47) |
| integrity/awful | floor | 8 | 1 | 4.62 s | 3.24 s–12.07 s | 12.41 s | +43% | 0.09 | 4/9 vs 2/9 (p 0.62) |

* significant after Holm correction over 95 comparisons (family error 0.05)
Last column: samples (fails count as slow) above twice the median of base, with Fisher's exact test.

## Faults found in the harness

- The command that starts the counter has the end marker in its text, and the
  terminal echoes the command. So the wait for the end marker returned at
  once, and the check "every line once, in order" saw only the lines up to
  then. The recovery times were right. The marker is now computed by the
  shell (`QC-END-$((6*7))`), and the checks cover the full run.
- Clients used fixed session names. A session that an earlier run left behind
  answered the next client with its old screen, which had the end marker. Now
  each client uses a new name.
- The harness found the prompt at the end of the screen stream. When the
  client drew an overlay (a "no reply" badge, a "back" note) after the
  prompt, the harness did not see the prompt, and the start counted as
  failed after 60 s. Three "failed" starts in the confirmatory run were of
  this kind; no check of the output failed. For qsh, the harness now also
  looks at the session stream (`QSH_DUMP`), which has no overlay.

## Reproduce

```sh
tests/netlab/lab.sh start                       # as root, on Linux
tests/netlab/ablate.py --rounds 6 --out r.jsonl  # all experiments and variants
tests/netlab/ablate.py --summary r.jsonl
# A part, or a sweep:
tests/netlab/ablate.py --only ctrlc/,flow/ --variants base,initwin=76800 --out s.jsonl
# Several labs at once:
QSH_LAB_NET=98 QSH_LAB_TUN=r tests/netlab/lab.sh start /tmp/qshlab2
tests/netlab/ablate.py --lab /tmp/qshlab2 --net 98 --only echo/ --out e.jsonl
```
