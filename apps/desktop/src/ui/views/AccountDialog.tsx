import { useEffect, useRef, useState, type FormEvent } from "react";
import { isTauriRuntime } from "../../bridge";
import { Button } from "../components/Button";
import { Dialog } from "../components/Dialog";
import { Icon } from "../Icons";
import { gameLaunchApi, launchError, type LocalIdentity } from "./game-launch-api";

export interface AccountDialogProps {
  open: boolean;
  onClose: () => void;
  onSelected?: (identity: LocalIdentity) => void;
}

const api = gameLaunchApi();

export interface LocalIdentityFormProps {
  username: string;
  pending: boolean;
  connected: boolean;
  onUsernameChange: (username: string) => void;
  onSubmit: (event: FormEvent) => void;
}

/** Native form controls keep validation and pending state accessible. */
export function LocalIdentityForm({ username, pending, connected, onUsernameChange, onSubmit }: LocalIdentityFormProps) {
  return (
    <form className="install-submit account-form" onSubmit={onSubmit}>
      <label className="install-field">本地玩家名称
        <input autoComplete="off" spellCheck={false} required disabled={pending} maxLength={16} pattern="[A-Za-z0-9_]{1,16}"
          value={username} onChange={(event) => onUsernameChange(event.target.value)} placeholder="字母、数字或下划线" />
      </label>
      <Button type="submit" variant="primary" disabled={!connected || !/^[A-Za-z0-9_]{1,16}$/.test(username)}
        pending={pending} pendingLabel="正在选择">使用此玩家</Button>
    </form>
  );
}

/** A local player is an explicit choice, separate from provider authentication. */
export function AccountDialog({ open, onClose, onSelected }: AccountDialogProps) {
  const [identity, setIdentity] = useState<LocalIdentity | null>(null);
  const [username, setUsername] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const generation = useRef(0);
  const connected = isTauriRuntime();

  useEffect(() => {
    const current = ++generation.current;
    if (!open || !connected) return;
    setError("");
    void api.currentIdentity().then((selected) => {
      if (generation.current !== current) return;
      setIdentity(selected);
      setUsername(selected?.username ?? "");
    }).catch((failure: unknown) => {
      if (generation.current === current) setError(launchError(failure));
    });
    return () => { generation.current += 1; };
  }, [open, connected]);

  async function select(event: FormEvent) {
    event.preventDefault();
    if (lock.current || !connected || !/^[A-Za-z0-9_]{1,16}$/.test(username)) return;
    const current = ++generation.current;
    lock.current = true;
    setPending(true);
    setError("");
    try {
      const selected = await api.selectOffline(username);
      if (generation.current === current) {
        setIdentity(selected);
        onSelected?.(selected);
        onClose();
      }
    } catch (failure) {
      if (generation.current === current) setError(launchError(failure));
    } finally {
      lock.current = false;
      setPending(false);
    }
  }

  return (
    <Dialog
      open={open}
      title="选择玩家"
      onClose={onClose}
    >
      <div className="account-intro">
        <span className="account-intro__avatar" aria-hidden="true"><Icon name="user" size={28} /></span>
        <div className="account-intro__copy">
          <strong>{identity ? identity.username : "你的游戏身份"}</strong>
          <p className="dialog__text">{identity ? "当前本地玩家" : "选择一个名称，开始单人游戏。"}</p>
        </div>
      </div>
      <LocalIdentityForm username={username} pending={pending} connected={connected}
        onUsernameChange={(value) => { generation.current += 1; setUsername(value); }}
        onSubmit={(event) => void select(event)} />
      {error ? <p role="alert" className="install-message">{error}</p> : null}
      {!connected ? <p className="field-note">请在桌面程序中选择玩家。</p> : null}
      <p className="field-note">本地玩家在退出启动器后清除。</p>
    </Dialog>
  );
}
