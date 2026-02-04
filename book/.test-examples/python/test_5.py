import eryx

session = eryx.Session()

# State persists across executions
session.execute("x = 42")
session.execute("y = x * 2")
result = session.execute("print(f'{x} * 2 = {y}')")
print(result.stdout)  # "42 * 2 = 84"