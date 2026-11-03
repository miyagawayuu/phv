# Same benchmarks as bench.phv, for comparison: python examples/bench/bench.py
import time


def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def loop_sum(n):
    total = 0
    for i in range(n):
        total += i % 7
    return total


def build_string(n):
    s = ""
    for _ in range(n):
        s += "x"
    return s


def mandelbrot(size):
    inside = 0
    for y in range(size):
        for x in range(size):
            cr = 2.5 * x / size - 2.0
            ci = 2.0 * y / size - 1.0
            zr = 0.0
            zi = 0.0
            k = 0
            while k < 50 and zr * zr + zi * zi <= 4.0:
                t = zr * zr - zi * zi + cr
                zi = 2.0 * zr * zi + ci
                zr = t
                k += 1
            if k == 50:
                inside += 1
    return inside


def concat_items(n):
    total = 0
    for i in range(n):
        # PHV prints 2.0 as "2"
        half = str(i // 2) if i % 2 == 0 else str(i * 0.5)
        s = "item-" + str(i) + ": " + half
        if s < "item-5":
            total += len(s)
    return total


def map_filter(n):
    return sum(x for x in map(lambda x: x * 2, range(1, n + 1)) if x % 3 == 0)


def count_keys(n):
    counts = {}
    for i in range(n):
        key = "k" + str(i % 1000)
        counts[key] = counts.get(key, 0) + 1
    return len(counts)


def ms(start):
    return f"{(time.perf_counter() - start) * 1000:,.1f} ms"


t = time.perf_counter()
r = fib(30)
print(f"fib(30) = {r}: {ms(t)}")

t = time.perf_counter()
r = loop_sum(10_000_000)
print(f"loop 10M: {r}: {ms(t)}")

t = time.perf_counter()
r = len(build_string(1_000_000))
print(f"append 1M chars ({r}): {ms(t)}")

t = time.perf_counter()
r = mandelbrot(300)
print(f"mandelbrot 300x300 ({r} inside): {ms(t)}")

t = time.perf_counter()
r = concat_items(1_000_000)
print(f"concat 1M ({r}): {ms(t)}")

t = time.perf_counter()
r = map_filter(1_000_000)
print(f"map/filter 1M ({r}): {ms(t)}")

t = time.perf_counter()
r = count_keys(200_000)
print(f"map updates 200k ({r} keys): {ms(t)}")
