# Same benchmarks as bench.phx, for comparison: python examples/bench/bench.py
import time


def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def ms(start):
    return f"{(time.perf_counter() - start) * 1000:,.1f} ms"


t = time.perf_counter()
r = fib(30)
print(f"fib(30) = {r}: {ms(t)}")

t = time.perf_counter()
total = 0
for i in range(10_000_000):
    total += i % 7
print(f"loop 10M: {total}: {ms(t)}")

t = time.perf_counter()
s = ""
for i in range(1_000_000):
    s += "x"
print(f"append 1M chars ({len(s)}): {ms(t)}")

t = time.perf_counter()
xs = list(range(1, 1_000_001))
total = sum(x for x in map(lambda x: x * 2, xs) if x % 3 == 0)
print(f"map/filter 1M ({total}): {ms(t)}")

t = time.perf_counter()
counts = {}
for i in range(200_000):
    key = "k" + str(i % 1000)
    counts[key] = counts.get(key, 0) + 1
print(f"map updates 200k ({len(counts)} keys): {ms(t)}")
