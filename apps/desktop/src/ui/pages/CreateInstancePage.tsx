import { Button } from "../components/Button";
import { GameInstallPanel } from "../views/GameInstallPanel";
import { PageHeader } from "../views/PageHeader";
import type { MotionPoint } from "../components/motion";
import type { CachedEnvironment } from "../views/environment-api";
import { CreateFromEnvironment } from "../views/CreateFromEnvironment";

export interface CreateInstancePageProps {
  onBack: () => void;
  onAccepted: (origin?: MotionPoint) => void;
  environment?: CachedEnvironment;
}

/** Creation uses the shared installation service; the instance list stays focused on games. */
export function CreateInstancePage({ onBack, onAccepted, environment }: CreateInstancePageProps) {
  return (
    <>
      <PageHeader
        headingId="create-instance-heading"
        icon="plus"
        title="创建实例"
        description="选一个游戏版本，为新的世界起个名字。"
        actions={<Button onClick={onBack}>返回实例</Button>}
      />
      <div className="creation-flow">{environment ? <CreateFromEnvironment key={environment.id} environment={environment} onAccepted={onAccepted} /> : <GameInstallPanel onAccepted={onAccepted} />}</div>
    </>
  );
}
