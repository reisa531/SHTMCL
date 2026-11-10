import type { AppInfo } from "../../contract";
import { pageCopy, shellCopy } from "../../ux/copy";
import {
  ACCENT_OPTIONS,
  DENSITY_OPTIONS,
  FONT_OPTIONS,
  MATERIAL_OPTIONS,
  MOTION_OPTIONS,
  THEME_OPTIONS,
  type ShellPreferences,
} from "../../ux/preferences";
import { Icon } from "../Icons";
import { uiLabels } from "../labels";
import { PageHeader } from "../views/PageHeader";

interface SegmentedGroupProps<T extends string> {
  name: string;
  legend: string;
  value: T;
  options: ReadonlyArray<{ value: T; label: string }>;
  onChange: (value: T) => void;
}

function SegmentedGroup<T extends string>({
  name,
  legend,
  value,
  options,
  onChange,
}: SegmentedGroupProps<T>) {
  return (
    <fieldset className="field-group">
      <legend className="field-group__legend">{legend}</legend>
      <div className="segmented">
        {options.map((option) => {
          const id = `${name}-${option.value}`;
          return (
            <label
              key={option.value}
              className={`segment${value === option.value ? " segment--on" : ""}`}
              htmlFor={id}
            >
              <input
                className="segment__input"
                id={id}
                type="radio"
                name={name}
                value={option.value}
                checked={value === option.value}
                onChange={() => onChange(option.value)}
              />
              <span className="segment__label">{option.label}</span>
            </label>
          );
        })}
      </div>
    </fieldset>
  );
}

export interface SettingsPageProps {
  preferences: ShellPreferences;
  onChange: (next: ShellPreferences) => void;
  appInfo: AppInfo | null;
  environment: "native" | "browser";
}

/** Preferences are local and fully wired; nothing here needs a core module. */
export function SettingsPage({
  preferences,
  onChange,
  appInfo,
  environment,
}: SettingsPageProps) {
  return (
    <>
      <PageHeader
        icon="settings"
        title={pageCopy.settings.title}
        description="外观与交互偏好。"
      />

      <div className="settings-layout">
      <section className="panel settings-section settings-panel" aria-labelledby="appearance-heading">
        <h2 className="settings-section__title" id="appearance-heading"><span className="settings-section__icon"><Icon name="settings" /></span>{shellCopy.settingsAppearance}</h2>
        <SegmentedGroup
          name="shtmcl-theme"
          legend={shellCopy.settingsTheme}
          value={preferences.theme}
          options={THEME_OPTIONS}
          onChange={(theme) => onChange({ ...preferences, theme })}
        />
        <SegmentedGroup
          name="shtmcl-material"
          legend="界面材质"
          value={preferences.material}
          options={MATERIAL_OPTIONS}
          onChange={(material) => onChange({ ...preferences, material })}
        />
        <SegmentedGroup
          name="shtmcl-accent"
          legend="强调色"
          value={preferences.accent}
          options={ACCENT_OPTIONS}
          onChange={(accent) => onChange({ ...preferences, accent })}
        />
        <SegmentedGroup
          name="shtmcl-font"
          legend="字体"
          value={preferences.font}
          options={FONT_OPTIONS}
          onChange={(font) => onChange({ ...preferences, font })}
        />
        <p className="field-note">更改即时生效，自动保存。</p>
      </section>

      <section className="panel settings-section settings-panel" aria-labelledby="experience-heading">
        <h2 className="settings-section__title" id="experience-heading"><span className="settings-section__icon"><Icon name="user" /></span>使用体验</h2>
        <SegmentedGroup
          name="shtmcl-density"
          legend={shellCopy.settingsDensity}
          value={preferences.density}
          options={DENSITY_OPTIONS}
          onChange={(density) => onChange({ ...preferences, density })}
        />
        <SegmentedGroup
          name="shtmcl-motion"
          legend={shellCopy.settingsMotion}
          value={preferences.motion}
          options={MOTION_OPTIONS}
          onChange={(motion) => onChange({ ...preferences, motion })}
        />
        <p className="field-note">减少动态效果时，页面保留清晰的状态变化。</p>
      </section>

      <section className="panel settings-section" aria-labelledby="about-heading">
        <h2 className="settings-section__title" id="about-heading"><span className="settings-section__icon"><Icon name="cloud" /></span>{shellCopy.settingsAbout}</h2>
        <dl className="about-list">
          <div className="about-list__row">
            <dt>{shellCopy.version}</dt>
            <dd>{appInfo ? `v${appInfo.version}` : shellCopy.unknown}</dd>
          </div>
          <div className="about-list__row">
            <dt>{shellCopy.environment}</dt>
            <dd>
              {environment === "native"
                ? shellCopy.environmentNative
                : shellCopy.environmentBrowser}
            </dd>
          </div>
          {/* Host facts come from `AppInfo`; the browser preview reports them as
              unknown instead of guessing an OS. */}
          <div className="about-list__row">
            <dt>{uiLabels.platform}</dt>
            <dd>{appInfo ? appInfo.platform : shellCopy.unknown}</dd>
          </div>
          <div className="about-list__row">
            <dt>{uiLabels.arch}</dt>
            <dd>{appInfo ? appInfo.arch : shellCopy.unknown}</dd>
          </div>
        </dl>
      </section>
      </div>
    </>
  );
}
