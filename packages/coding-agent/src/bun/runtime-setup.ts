import { bedrockProviderModule } from "@dum-e/ai/bedrock-provider";
import { registerBunOAuthFlows } from "@dum-e/ai/bun-oauth";
import { setBedrockProviderModule } from "@dum-e/ai/compat";
import { APP_NAME } from "../config.ts";

process.title = APP_NAME;
process.emitWarning = (() => {}) as typeof process.emitWarning;
registerBunOAuthFlows();
setBedrockProviderModule(bedrockProviderModule);
