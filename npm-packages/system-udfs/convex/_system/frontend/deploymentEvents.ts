import { Doc } from "../../_generated/dataModel";
import { queryPrivateSystem } from "../secretSystemTables";
import { v } from "convex/values";

type DeployEvent = Doc<"_deployment_audit_log"> & {
  action:
    | "deploy"
    | "deploy_with_components"
    | "push_config"
    | "push_config_with_components";
};

export const lastDeployEvent = queryPrivateSystem({
  args: {
    componentId: v.optional(v.union(v.string(), v.null())),
  },
  handler: async function ({ db }): Promise<DeployEvent | null> {
    const lastDeployEvent = await db
      .query("_deployment_audit_log")
      .filter((q) =>
        q.or(
          q.eq(q.field("action"), "deploy"),
          q.eq(q.field("action"), "deploy_with_components"),
          q.eq(q.field("action"), "push_config"),
          q.eq(q.field("action"), "push_config_with_components"),
        ),
      )
      .order("desc")
      .first();

    // Making the type system happy. This should never happen
    // because the query should always return a push event.
    if (
      lastDeployEvent &&
      lastDeployEvent.action !== "deploy" &&
      lastDeployEvent.action !== "deploy_with_components" &&
      lastDeployEvent.action !== "push_config" &&
      lastDeployEvent.action !== "push_config_with_components"
    ) {
      return null;
    }

    return lastDeployEvent;
  },
});
