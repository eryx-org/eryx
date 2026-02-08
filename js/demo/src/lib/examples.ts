export const RUN_EXAMPLES: Record<string, { label: string; code: string }> = {
  hello: {
    label: "Hello World",
    code: `print("Hello from eryx!")
print("Python 3.14 in WebAssembly!")`,
  },
  math: {
    label: "Math",
    code: `# Arithmetic
result = (2 ** 10) + (3 * 7) - 1
print(f"2^10 + 3*7 - 1 = {result}")

# Float
pi_approx = 355 / 113
print(f"Pi approx: {pi_approx:.10f}")`,
  },
  fibonacci: {
    label: "Fibonacci",
    code: `def fibonacci(n):
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a

for i in range(15):
    print(f"fib({i:2d}) = {fibonacci(i)}")`,
  },
  strings: {
    label: "Strings",
    code: `name = "eryx"
greeting = f"Hello, {name}!"
print(greeting)
print(greeting.upper())
print(greeting[::-1])

words = "the quick brown fox".split()
print(" ".join(w.capitalize() for w in words))`,
  },
  lists: {
    label: "Lists",
    code: `squares = [x ** 2 for x in range(10)]
print(f"Squares: {squares}")

evens = [x for x in squares if x % 2 == 0]
print(f"Even squares: {evens}")

matrix = [[i * 3 + j for j in range(3)] for i in range(3)]
for row in matrix:
    print(row)`,
  },
  classes: {
    label: "Classes",
    code: `class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def distance_to(self, other):
        return ((self.x - other.x)**2 + (self.y - other.y)**2) ** 0.5

    def __repr__(self):
        return f"Point({self.x}, {self.y})"

p1 = Point(0, 0)
p2 = Point(3, 4)
print(f"{p1} -> {p2}")
print(f"Distance: {p1.distance_to(p2)}")`,
  },
  stdlib: {
    label: "Stdlib",
    code: `import json, re, hashlib, base64, datetime, pickle

data = json.loads('{"key": "value", "n": 42}')
encoded = base64.b64encode(b"hello eryx").decode()
hash_val = hashlib.md5(b"test").hexdigest()[:8]
match = re.search(r'\\d+', 'abc123def')
now = datetime.datetime.now()
pickled = pickle.dumps(data)

print(f"json:    {data}")
print(f"base64:  {encoded}")
print(f"md5:     {hash_val}")
print(f"regex:   {match.group()}")
print(f"time:    {now}")
print(f"pickle:  {len(pickled)} bytes")`,
  },
};

export const SESSION_EXAMPLES: Record<string, { label: string; code: string }> =
  {
    define: {
      label: "Define Variables",
      code: `x = 42
y = "hello"
items = [1, 2, 3]
print(f"x={x}, y={y}, items={items}")`,
    },
    check: {
      label: "Check State",
      code: `print(f"x={x}, y={y}, items={items}")`,
    },
    modify: {
      label: "Modify State",
      code: `x += 1
items.append(len(items) + 1)
print(f"x={x}, items={items}")`,
    },
    function: {
      label: "Define Function",
      code: `def greet(name):
    return f"Hello, {name}!"
print(greet("World"))`,
    },
    call: {
      label: "Call Function",
      code: `print(greet("eryx"))`,
    },
  };

export const FS_EXAMPLES: Record<string, { label: string; code: string }> = {
  list: {
    label: "List Files",
    code: `import os

# List all files in /data
for entry in os.listdir('/data'):
    full = os.path.join('/data', entry)
    kind = 'dir' if os.path.isdir(full) else 'file'
    size = os.path.getsize(full) if os.path.isfile(full) else 0
    print(f"  [{kind:4s}] {entry:30s} {size:>8d} bytes")`,
  },
  readwrite: {
    label: "Read & Write",
    code: `# Write a file
with open('/data/hello.txt', 'w') as f:
    f.write('Hello from the eryx sandbox!\\n')
    f.write('This file lives in the virtual filesystem.\\n')

# Read it back
with open('/data/hello.txt', 'r') as f:
    content = f.read()

print("Wrote and read /data/hello.txt:")
print(content)`,
  },
  csv: {
    label: "CSV Processing",
    code: `import csv, io

# Write a CSV file
with open('/data/people.csv', 'w', newline='') as f:
    writer = csv.writer(f)
    writer.writerow(['Name', 'Age', 'City'])
    writer.writerow(['Alice', 30, 'London'])
    writer.writerow(['Bob', 25, 'Paris'])
    writer.writerow(['Charlie', 35, 'Tokyo'])

# Read it back
with open('/data/people.csv', 'r') as f:
    reader = csv.DictReader(f)
    for row in reader:
        print(f"{row['Name']:10s} age {row['Age']:>3s} from {row['City']}")`,
  },
  filestats: {
    label: "File Stats",
    code: `import os, datetime

path = '/data'
print(f"Contents of {path}:\\n")
for name in sorted(os.listdir(path)):
    full = os.path.join(path, name)
    stat = os.stat(full)
    is_dir = os.path.isdir(full)
    size = stat.st_size
    mtime = datetime.datetime.fromtimestamp(stat.st_mtime)
    prefix = 'd' if is_dir else '-'
    print(f"  {prefix} {size:>8d}  {mtime:%Y-%m-%d %H:%M}  {name}")`,
  },
  tree: {
    label: "Directory Tree",
    code: `import os

def print_tree(path, prefix=''):
    entries = sorted(os.listdir(path))
    dirs = [e for e in entries if os.path.isdir(os.path.join(path, e))]
    files = [e for e in entries if os.path.isfile(os.path.join(path, e))]
    all_entries = dirs + files
    for i, entry in enumerate(all_entries):
        is_last = i == len(all_entries) - 1
        connector = '\\u2514\\u2500\\u2500 ' if is_last else '\\u251c\\u2500\\u2500 '
        full = os.path.join(path, entry)
        if os.path.isdir(full):
            print(f"{prefix}{connector}{entry}/")
            ext = '    ' if is_last else '\\u2502   '
            print_tree(full, prefix + ext)
        else:
            size = os.path.getsize(full)
            print(f"{prefix}{connector}{entry} ({size}B)")

print("/data/")
print_tree('/data')`,
  },
};
