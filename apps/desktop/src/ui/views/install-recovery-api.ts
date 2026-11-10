import { dispatchCoreMutation } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

/** Only source identity crosses the bridge; installation choices come from disk. */
export async function retryInstallation(operationId: string, taskId: string,
  dispatch: (request: CommandRequest) => Promise<CommandResponse> = dispatchCoreMutation): Promise<string> {
  const nextOperation = crypto.randomUUID();
  const response = await dispatch({requestId: crypto.randomUUID(), operationId: nextOperation,
    payload: {command: "launch.installations.retry", sourceOperationId: operationId, sourceTaskId: taskId}});
  if (response.type !== "accepted" || response.operationId !== nextOperation || !response.taskId) {
    throw new Error("Expected installation recovery task");
  }
  return response.taskId;
}
