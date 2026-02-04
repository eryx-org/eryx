import eryx

def get_time():
    import time
    return {"timestamp": time.time()}

sandbox = eryx.Sandbox(
    callbacks=[
        {
            "name": "get_time",
            "fn": get_time,
            "description": "Returns current Unix timestamp"
        }
    ]
)

result = sandbox.execute("""
# Callbacks are available as async functions
t = await get_time()
print(f"Time: {t['timestamp']}")
""")

print(result.stdout)