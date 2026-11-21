// {{name}} tool: one read through the guest's egress proxy. Runs under Bun.
//
// The tool holds no credential. It names the connector's placeholder and the node puts the
// member's credential in its place, for the host and path this skill declares (manifest.json, egress).

const PLACEHOLDER = "vamsec_{{id}}_api";
const HOST = "{{host}}";

async function host(): Promise<string> {
  // A company address connector: the node writes the member's host beside this file.
  if (!HOST.includes("{subdomain}")) return HOST;
  try {
    const file = JSON.parse(await Bun.file(`${import.meta.dir}/.vam/connector.json`).text());
    return String(file.hosts.api);
  } catch {
    return "";
  }
}

async function main(): Promise<number> {
  const raw = (await Bun.stdin.text()).trim();
  const args: Record<string, unknown> = raw ? JSON.parse(raw) : {};
  const target = await host();
  if (!target) {
    console.error("{{id}}-error: no host to call");
    return 1;
  }
  const query = args.query ? `?query=${encodeURIComponent(String(args.query))}` : "";
  const res = await fetch(process.env.VAM_EGRESS_ENDPOINT || "http://127.0.0.1:3128", {
    method: "GET",
    headers: {
      "X-Target-Url": `https://${target}/v1/items${query}`,
      Authorization: PLACEHOLDER,
      Accept: "application/json",
    },
  });
  const body = await res.text();
  if (res.status < 200 || res.status >= 300) {
    console.error(`{{id}}-error: answered ${res.status}`);
    return 1;
  }
  console.log(body);
  return 0;
}

process.exit(await main());
