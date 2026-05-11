import type { ReactNode } from "react";
import type { NavEntry } from "../App";

interface Props {
  active: NavEntry;
  pill?: { text: string; tone: "ok" | "err" | "idle" } | null;
  meta?: ReactNode;
}

export function TabHeader({ active, pill, meta }: Props) {
  return (
    <header className="tab-header">
      <div className="tab-header-lead">
        <span className="tab-header-num">{active.num}</span>
        <div className="tab-header-titles">
          <span className="tab-header-cn">
            {active.cn}
            {pill && (
              <span className={`tab-header-pill ${pill.tone}`}>
                <span className="dot" />
                {pill.text}
              </span>
            )}
          </span>
          <span className="tab-header-en">{active.en}</span>
        </div>
      </div>
      {meta && <div className="tab-header-meta">{meta}</div>}
    </header>
  );
}
