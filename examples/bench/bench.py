# Same benchmarks as bench.phv, for comparison: python examples/bench/bench.py
import functools
import html
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


def sieve(n):
    prime = []
    for _ in range(n + 1):
        prime.append(True)
    count = 0
    for i in range(2, n + 1):
        if prime[i]:
            count += 1
            for j in range(i * i, n + 1, i):
                prime[j] = False
    return count


def list_sum(n):
    xs = []
    for i in range(n):
        xs.append(i * 0.5)
    t = 0.0
    for x in xs:
        t += x
    return t


class Builder:
    def __init__(self):
        self.text = ""


def prop_append(n):
    b = Builder()
    for _ in range(n):
        b.text += "x"
    return len(b.text)


def string_equals(n):
    words = ["alpha", "beta", "gamma", "delta"]
    hits = 0
    for i in range(n):
        if words[i % 4] == "gamma":
            hits += 1
    return hits


def html_escape_loop(n):
    total = 0
    for i in range(n):
        total += len(html.escape(f'Tom & Jerry <b>said</b> "hello" to {i} friends today'))
    return total


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y


def new_objects(n):
    total = 0
    for i in range(n):
        p = Point(i, 1)
        total += p.y
    return total


def throw_catch(n):
    caught = 0
    for _ in range(n):
        try:
            raise RuntimeError("failed")
        except RuntimeError:
            caught += 1
    return caught


def sort_by(n):
    xs = [(i * 7919) % n for i in range(n)]
    return sorted(xs, key=functools.cmp_to_key(lambda a, b: (b > a) - (b < a)))[0]


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

t = time.perf_counter()
r = sieve(1_000_000)
print(f"sieve 1M ({r} primes): {ms(t)}")

t = time.perf_counter()
r = list_sum(1_000_000)
print(f"list of 1M floats ({r}): {ms(t)}")

t = time.perf_counter()
r = prop_append(200_000)
print(f"property .= 200k ({r}): {ms(t)}")

t = time.perf_counter()
r = string_equals(1_000_000)
print(f"string == 1M ({r}): {ms(t)}")

t = time.perf_counter()
r = html_escape_loop(200_000)
print(f"escape() 200k ({r}): {ms(t)}")

t = time.perf_counter()
r = new_objects(1_000_000)
print(f"new 1M ({r}): {ms(t)}")

t = time.perf_counter()
r = throw_catch(100_000)
print(f"throw/catch 100k ({r}): {ms(t)}")

t = time.perf_counter()
r = sort_by(200_000)
print(f"sortBy 200k ({r}): {ms(t)}")
