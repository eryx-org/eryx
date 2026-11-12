/**
 * Sandboxed code calling functions in this process: an agent's tools, say.
 *
 *     ERYX_API_KEY=eryx_... node examples/callbacks.ts
 */

import { Sandbox } from "@eryx-org/eryx-run";

const users: Record<number, { name: string; email: string }> = {
  1: { name: "Ada", email: "ada@example.com" },
  2: { name: "Grace", email: "grace@example.com" },
};
const sent: string[] = [];

const sandbox = new Sandbox({
  callbacks: {
    get_user: {
      description: "Look up a user by id",
      handler: ({ id }) => {
        const user = users[id];
        // Throwing raises a RuntimeError in the sandboxed code
        if (!user) throw new Error(`no user ${id}`);
        return user;
      },
    },
    send_email: {
      description: "Send an email",
      handler: async ({ to, subject }) => {
        sent.push(`${to}: ${subject}`);
        return { ok: true };
      },
    },
  },
});

// The code is what an LLM might write: it can only reach what you pass in
const result = await sandbox.execute(`
import asyncio

users = await asyncio.gather(get_user(id=1), get_user(id=2))
for user in users:
    await send_email(to=user["email"], subject=f"Hi {user['name']}")

try:
    await get_user(id=3)
except RuntimeError as e:
    print("lookup failed:", e)

result = [user["name"] for user in users]
`);

console.log(result.stdout.trimEnd());
console.log("result:", result.result);
console.log("emails:", sent);
console.log(`${result.callbackInvocations} callbacks, ${result.fuelConsumed} compute units`);
