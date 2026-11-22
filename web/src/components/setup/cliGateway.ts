// The minimal cli-gateway commands of docs/onboarding.md §4, filled with this core's values.
// One place builds them so the gateway and conversation steps show the same commands.

export const GATEWAYS_REPO = 'https://github.com/kojira/opencrab-gateways';
export const BLUESKY_GATEWAY_REPO = 'https://github.com/kojira/opencrab-bluesky-gateway';
export const CLI_GATEWAY_README = `${GATEWAYS_REPO}/blob/main/crates/cli-gateway/README.md`;
export const ONBOARDING_DOC = 'https://github.com/kojira/opencrab/blob/main/docs/onboarding.md';

/** The example instance id of docs/onboarding.md §4.2 (any lowercase UUID works). */
const INSTANCE_ID = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';

export function buildCommand(): string {
  return [
    `git clone ${GATEWAYS_REPO}.git`,
    'cd opencrab-gateways',
    'cargo +1.98.0 build -p opencrab-cli-gateway --locked',
  ].join('\n');
}

export function placementCommand(socket: string, agentId: string): string {
  const placement = {
    core_socket: socket,
    instances: [{ instance_id: INSTANCE_ID, agent_id: agentId, author_id: 'local-operator' }],
  };
  return `cat > cli-placement.json <<'EOF'\n${JSON.stringify(placement, null, 2)}\nEOF`;
}

export function replCommand(agentId: string): string {
  return `./target/debug/opencrab-cli-gateway --placement cli-placement.json --agent ${agentId} --new "Terminal chat" --mode repl`;
}
