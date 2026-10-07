# Network simulation

Runs the real host and viewer (mock capture/codec) through `relay.py`, a
UDP relay that adds delay, jitter, loss, stalls, reordering and a bandwidth
cap, then checks every use case. Linux or macOS, after
`cargo build --release -p aa-host -p aa-viewer`:

    tools/sim/matrix.sh        # full pass, writes tools/sim/report.md (~9 min)
    tools/sim/run.sh NAME 1280x720 120 "4 2 0.005 5 40" 15   # one run
    (cd tools/sim && python3 summarize.py NAME)

relay.py arguments: base_ms jitter_ms loss_fraction stall_every_s stall_ms
[reorder_fraction] [rate_mbps].
