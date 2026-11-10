import { useEffect, useState } from "react";
import { modMetadataApi, type ModMetadata } from "../views/mod-metadata-api";
import { Button } from "./Button";

const statusText = {
  unsupported: "未识别到受支持的 Fabric 声明；其他加载器的元数据暂不解析。",
  invalid: "文件或模组声明无法解析，不能据此判断兼容性。",
  limited: "文件或声明超过读取上限，暂不解析。",
  unavailable: "无法读取所选文件，请刷新清单后重试。",
  busy: "正在读取其他模组，请稍后重试。",
};

export function ModMetadataDetails({ instanceId, fileName }: { instanceId: string; fileName: string }) {
  const [value, setValue] = useState<ModMetadata | null>(null);
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    let active = true;
    setValue(null);
    void modMetadataApi().inspect(instanceId, fileName).then((result) => { if (active) setValue(result); })
      .catch(() => { if (active) setValue({ status: "unavailable" }); });
    return () => { active = false; };
  }, [instanceId, fileName, revision]);
  if (!value) return <p role="status">正在读取模组声明…</p>;
  if (value.status !== "ready") return <div className="instance-detail__note">
    <p role="status">{statusText[value.status]}</p>
    <Button size="sm" onClick={() => setRevision(value => value + 1)}>重试读取声明</Button>
  </div>;
  return <section aria-label="模组文件声明" className="mod-metadata">
    <h3>文件内声明</h3>
    <dl className="instance-summary">
      <div><dt>模组名称</dt><dd>{value.name ?? "未声明"}</dd></div>
      <div><dt>模组 ID</dt><dd>{value.id}</dd></div>
      <div><dt>模组版本</dt><dd>{value.version}</dd></div>
      <div><dt>加载器声明</dt><dd>Fabric</dd></div>
      <div><dt>运行环境声明</dt><dd>{value.environment === "*" ? "客户端与服务端" : value.environment === "client" ? "客户端" : value.environment === "server" ? "服务端" : "未声明"}</dd></div>
    </dl>
    <h4>必需依赖声明</h4>
    {value.dependencies.length ? <ul>{value.dependencies.map((dependency) => <li key={dependency.id}>
      <code>{dependency.id}</code>：{dependency.versions.join(" 或 ")}
    </li>)}</ul> : <p>未声明必需依赖。</p>}
    <p className="settings-note">来源：fabric.mod.json。以上为文件内声明，尚未核对当前实例的版本、依赖或兼容性。</p>
  </section>;
}
