import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("print('Hello from Eryx!')")
print(result.stdout)