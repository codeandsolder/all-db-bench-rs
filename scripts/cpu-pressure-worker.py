#!/usr/bin/env python3
import signal
import sys
import time

if len(sys.argv) != 2:
    raise SystemExit("usage: cpu-pressure-worker.py DUTY_PERCENT")

duty = float(sys.argv[1])
if not 0.0 < duty <= 100.0:
    raise SystemExit("DUTY_PERCENT must be in (0, 100]")

period_s = 0.020
busy_s = period_s * duty / 100.0
sleep_s = period_s - busy_s
running = True

def stop(_signum, _frame):
    global running
    running = False

signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)

while running:
    deadline = time.perf_counter() + busy_s
    while running and time.perf_counter() < deadline:
        pass
    if running and sleep_s > 0:
        time.sleep(sleep_s)
