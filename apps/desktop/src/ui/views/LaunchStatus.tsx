import type { Task } from "../../contract";
import { Banner } from "../components/Banner";
import { Button } from "../components/Button";
import { launchError } from "./game-launch-api";
import { canCancel, isTerminalState, type CancelFeedback } from "./task-format";
import type { useLaunchFlow } from "./use-launch-flow";

const PHASES: Record<string, string> = {
  snapshot: "读取实例", resource_prepare: "准备游戏资源", auth: "检查玩家",
  runtime: "准备 Java", files: "检查游戏文件", spawn: "启动游戏进程",
};

export function LaunchStatus({ flow, onRefresh, refreshing, onCancel, cancelling, feedback }: {
  flow: ReturnType<typeof useLaunchFlow>;
  onRefresh: () => void;
  refreshing: boolean;
  onCancel: (task: Task) => void;
  cancelling: boolean;
  feedback?: CancelFeedback;
}) {
  const { intent, task } = flow;
  if (!intent) return null;
  const error = intent.error || (task?.state === "failed" ? launchError(task.error) : "");
  let body = "正在准备启动…";
  if (intent.stage === "identity") body = "选择玩家后将继续启动此实例。";
  if (intent.stage === "accepted") body = task ? PHASES[task.phase] ?? "正在准备启动…" : "正在获取启动进度。";
  if (task?.state === "succeeded") body = "游戏进程已启动。";
  if (task?.state === "cancelled") body = "已取消启动。";
  if (task && !isTerminalState(task.state) && (task.cancelRequested || feedback?.status === "requested")) body = "正在取消启动…";
  if (error) body = error;
  if (feedback?.status === "rejected") body += ` ${feedback.reason}`;
  const terminal = intent.stage === "error" || (task && isTerminalState(task.state));
  return <div className="app-shell__banner" data-launch-status={task?.state ?? intent.stage}>
    <Banner tone={error ? "error" : task?.state === "succeeded" ? "success" : "info"}
      title={intent.game.name} body={body} actions={<>
        {task && canCancel(task) ? <Button size="sm" pending={cancelling} onClick={() => onCancel(task)}>取消启动</Button> : null}
        {intent.stage === "accepted" && !terminal ? <Button size="sm" pending={refreshing} onClick={onRefresh}>刷新进度</Button> : null}
        {terminal ? <Button size="sm" onClick={flow.dismiss}>收起</Button> : null}
      </>} />
  </div>;
}
