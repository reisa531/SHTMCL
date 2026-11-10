import type { InstalledGame } from "../views/game-install-api";
import { loaderLabel } from "./InstanceArtwork";

/** Shared identity and inventory; missing counts remain unknown, never zero. */
export function InstanceDetails({ game }: { game: InstalledGame }) {
  const { version, presentation } = game;
  return <>
    <span>Minecraft {version.gameVersion}</span>
    <span>{loaderLabel(version.loader)}{version.loaderVersion ? ` ${version.loaderVersion}` : ""}</span>
    {presentation?.modFileCount !== undefined && (presentation.modFileCount > 0 || version.loader !== "vanilla")
      ? <span>{presentation.modFileCount} 个模组文件</span> : null}
    {presentation?.modpackName ? <span>整合包 · {presentation.modpackName}</span> : null}
  </>;
}
