import { useId } from "react";

/** Decorative, original vector scenery; no large cover image or external request. */
export function WorldScene() {
  const id = useId();
  return <svg className="world-scene" viewBox="0 0 1000 330" preserveAspectRatio="xMidYMid slice" aria-hidden="true" focusable="false">
    <defs>
      <linearGradient id={`${id}-sky`} x2="0" y2="1"><stop stopColor="#849bc4" /><stop offset=".65" stopColor="#d9ab9f" /><stop offset="1" stopColor="#f4c19b" /></linearGradient>
      <linearGradient id={`${id}-foreground`} x2="0" y2="1"><stop stopColor="#436b58" /><stop offset="1" stopColor="#203e3b" /></linearGradient>
    </defs>
    <path fill={`url(#${id}-sky)`} d="M0 0h1000v330H0z" />
    <circle cx="748" cy="113" r="37" fill="#ffe0b4" opacity=".86" />
    <path d="M0 188h90v-23h80v-27h60v22h95v32h90v-30h64v-23h87v29h86v24h81v-32h105v-21h85v26h77v165H0Z" fill="#84908b" />
    <path d="M0 227h120v-23h104v-26h63v21h96v34h96v-19h66v-26h100v23h105v-20h88v-18h72v35h90v122H0Z" fill="#5c7c6b" />
    <path d="M0 264h90v-17h118v25h123v-24h95v-19h89v19h118v21h107v-22h106v-21h88v20h66v84H0Z" fill={`url(#${id}-foreground)`} />
    {[{ x: 55, y: 176 }, { x: 188, y: 210 }, { x: 560, y: 216 }, { x: 665, y: 177 }, { x: 863, y: 193 }, { x: 940, y: 229 }].map(({ x, y }) => <g key={x} transform={`translate(${x} ${y})`}><path d="M-4 0h8v55h-8Z" fill="#594a3b" /><path d="M-23 0h46v29h-46zM-16-15h32V8h-32z" fill="#325c4b" /><path d="M-23 0h23v29h-23z" fill="#46765a" /></g>)}
    <path d="M0 315h183v-11h117v12h181v-9h157v23H0Z" fill="#203e37" />
  </svg>;
}
