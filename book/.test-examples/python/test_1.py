import eryx

# Create a sandbox with embedded Python runtime
sandbox = eryx.Sandbox()

# Execute Python code in isolation
result = sandbox.execute('''
print("Hello from the sandbox!")
x = 2 + 2
print(f"2 + 2 = {x}")
''')

print(result.stdout)
# Output:
# Hello from the sandbox!
# 2 + 2 = 4

print(f"Execution took {result.duration_ms:.2f}ms")