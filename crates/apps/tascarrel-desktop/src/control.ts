import { randomBytes } from "node:crypto";
import { connect } from "node:net";

const CONTROL_REQUEST_TIMEOUT_MS = 5_000;
const ID_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

interface RpcMessage {
  protocol?: string;
  content?: {
    type?: string;
    id?: string;
    output?: { pairingKey?: string };
    error?: unknown;
  };
}

export function createPairingKey(socketPath: string, label: string): Promise<string> {
  const invocationId = `invocation_${randomIdentifierSuffix()}`;
  const request = {
    protocol: "Rpc",
    content: {
      type: "Invoke",
      id: invocationId,
      target: { type: "Host" },
      procedure: "auth_CreatePairingKey",
      input: { label },
    },
  };

  return new Promise((resolve, reject) => {
    const socket = connect(socketPath);
    let buffered = Buffer.alloc(0);
    let settled = false;
    const timer = setTimeout(
      () => fail(new Error("Pairing RPC timed out")),
      CONTROL_REQUEST_TIMEOUT_MS,
    );
    const fail = (cause: unknown) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      reject(new Error("Failed to create a desktop browser pairing key", { cause }));
    };
    const succeed = (pairingKey: string) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.end();
      resolve(pairingKey);
    };

    socket.once("error", fail);
    socket.once("close", () => {
      if (!settled) fail(new Error("Pairing RPC connection closed without a response"));
    });
    socket.once("connect", () => {
      const payload = Buffer.from(JSON.stringify(request));
      const frame = Buffer.allocUnsafe(4 + payload.length);
      frame.writeUInt32BE(payload.length, 0);
      payload.copy(frame, 4);
      socket.write(frame);
    });
    socket.on("data", (chunk: Buffer) => {
      buffered = Buffer.concat([buffered, chunk]);
      while (buffered.length >= 4) {
        const length = buffered.readUInt32BE(0);
        if (buffered.length < 4 + length) return;
        const payload = buffered.subarray(4, 4 + length);
        buffered = buffered.subarray(4 + length);
        let message: RpcMessage;
        try {
          message = JSON.parse(payload.toString("utf8")) as RpcMessage;
        } catch (cause) {
          fail(cause);
          return;
        }
        if (message.protocol !== "Rpc" || message.content?.id !== invocationId) continue;
        if (message.content.type === "Completed" && message.content.output?.pairingKey) {
          succeed(message.content.output.pairingKey);
          return;
        }
        fail(new Error(`Pairing RPC failed: ${JSON.stringify(message.content?.error)}`));
        return;
      }
    });
  });
}

function randomIdentifierSuffix(): string {
  const bytes = randomBytes(22);
  let suffix = "";
  for (const byte of bytes) suffix += ID_ALPHABET[byte % ID_ALPHABET.length];
  return suffix;
}
