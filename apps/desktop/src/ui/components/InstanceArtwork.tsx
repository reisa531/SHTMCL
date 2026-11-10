/** Small original marks keep the library recognisable without downloading artwork. */
export function loaderLabel(loader: string): string {
  const names: Record<string, string> = { vanilla: "原版", forge: "Forge", neoforge: "NeoForge", fabric: "Fabric", optifine: "OptiFine", quilt: "Quilt" };
  return names[loader.toLowerCase()] ?? loader;
}

export function InstanceArtwork({ loader = "vanilla" }: { loader?: string }) {
  const kind = loader.toLowerCase();
  return <span className="instance-artwork" data-loader={kind} aria-hidden="true">
    <svg viewBox="0 0 64 64" fill="none">
      {kind === "vanilla" ? <>
        <path d="m32 5 25 13-25 14L7 18 32 5Z" fill="#86b75b" />
        <path d="M7 18v28l25 14V32L7 18Z" fill="#876344" />
        <path d="M32 32v28l25-14V18L32 32Z" fill="#654b37" />
        <path d="m7 18 25 14 25-14v10L32 42 7 28V18Z" fill="#548747" />
        <path d="m18 12 14 7 12-7M18 25v9m25-7v10M13 37l7 4m19 6 8-5" stroke="#ffffff" strokeOpacity=".15" strokeWidth="4" />
      </> : kind === "forge" ? <>
        <path d="M8 18h48l-9 10H33l-3 11 14 6v7H15v-7l10-6-4-11H8V18Z" fill="#66717e" />
        <path d="M8 15h48v7H8z" fill="#9da8b4" /><path d="M16 48h28v5H16z" fill="#414d5c" />
      </> : kind === "neoforge" ? <>
        <path d="m12 11 15 9h10l15-9-3 27-17 17-17-17-3-27Z" fill="#d68c4f" />
        <path d="m17 20 10 10h10l10-10-5 22-10 9-10-9-5-22Z" fill="#ffe0b3" />
        <path d="m27 37 5 5 5-5H27Z" fill="#493e3b" /><path d="M23 30h3m12 0h3" stroke="#493e3b" strokeWidth="4" />
      </> : kind === "fabric" || kind === "quilt" ? <>
        <path d="M17 10h30v44H17z" fill="#d1bd98" /><path d="M17 16h30M17 48h30" stroke="#9c8867" strokeWidth="4" />
        <path d="M24 25h16v15H24z" stroke="#8d7a5c" strokeDasharray="3 3" strokeWidth="2" /><path d="m28 31 4 4 7-8" stroke="#756448" strokeWidth="3" />
      </> : <>
        <path d="m32 5 24 14v27L32 60 8 46V19L32 5Z" fill={kind === "optifine" ? "#82aacb" : "#9e98bd"} />
        <path d="m20 32 9 9 16-19" stroke="white" strokeWidth="5" strokeLinecap="round" strokeLinejoin="round" />
      </>}
    </svg>
  </span>;
}
