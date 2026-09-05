// Board surface: token-authenticated `/v1/board/*` browser (open board).
// Session-scoped board (RightRail) uses `/v1/sessions/{id}/board*` via getBoard().
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  boardWhoami,
  fetchTokenBoardAttachment,
  getStoredBoardSpace,
  getStoredBoardToken,
  getTokenBoard,
  getTokenBoardItem,
  setStoredBoardSpace,
  setStoredBoardToken,
  tokenBoardComment,
  tokenBoardTransition,
  type Board,
  type BoardItemDetail,
} from "../api";
import { BoardOverlay, BoardSection } from "./BoardPanel";
import { Icon } from "./Icon";

export function BoardView() {
  const { t } = useTranslation();
  const [token, setToken] = useState(() => getStoredBoardToken());
  const [space, setSpace] = useState(() => getStoredBoardSpace());
  const [who, setWho] = useState<{ actor: string; role: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [board, setBoard] = useState<Board | null>(null);
  const [overlayOpen, setOverlayOpen] = useState(false);
  const [detailId, setDetailId] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async (tok: string, sp: string) => {
    if (!tok.trim() || !sp.trim()) {
      setBoard(null);
      setWho(null);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const me = await boardWhoami(tok);
      if ("error" in me && me.error) {
        setWho(null);
        setBoard(null);
        setError(me.error);
        return;
      }
      setWho(me as { actor: string; role: string });
      const next = await getTokenBoard(sp.trim(), tok);
      setBoard(next);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBoard(null);
      setWho(null);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void refresh(token, space);
  }, [token, space, refresh]);

  const saveCreds = () => {
    setStoredBoardToken(token.trim());
    setStoredBoardSpace(space.trim());
    void refresh(token.trim(), space.trim());
  };

  const loadItem = (id: number): Promise<BoardItemDetail | { error: string }> =>
    getTokenBoardItem(space.trim(), id, token.trim());

  const loadAttachment = (stored: string): Promise<string | null> =>
    fetchTokenBoardAttachment(space.trim(), stored, token.trim());

  return (
    <div className="board-view" data-testid="board-view">
      <div className="board-view-head">
        <Icon name="table" size={16} />
        <span className="board-view-title">{t("rail.board_title")}</span>
        {who && (
          <span className="board-overlay-space" data-testid="board-whoami">
            {who.actor} · {who.role}
          </span>
        )}
        <span className="spacer" />
        <button
          className="btn"
          data-testid="board-refresh"
          disabled={busy || !token.trim() || !space.trim()}
          onClick={() => void refresh(token, space)}
        >
          {t("rail.refresh")}
        </button>
      </div>

      <div className="board-view-creds">
        <label className="board-view-field">
          <span>{t("board.token_label")}</span>
          <input
            data-testid="board-token"
            type="password"
            autoComplete="off"
            placeholder={t("board.token_placeholder")}
            value={token}
            onChange={(e) => setToken(e.target.value)}
          />
        </label>
        <label className="board-view-field">
          <span>{t("board.space_label")}</span>
          <input
            data-testid="board-space"
            placeholder={t("board.space_placeholder")}
            value={space}
            onChange={(e) => setSpace(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") saveCreds();
            }}
          />
        </label>
        <button className="btn primary" data-testid="board-connect" onClick={saveCreds}>
          {t("board.connect")}
        </button>
      </div>

      {error && (
        <div className="board-view-error" data-testid="board-error">
          {error}
        </div>
      )}

      {board?.space && (
        <div className="board-view-body">
          <div className="board-view-rail">
            <div className="board-view-rail-head">
              <span>{board.name}</span>
              <button
                className="btn"
                data-testid="board-expand"
                onClick={() => {
                  setDetailId(null);
                  setOverlayOpen(true);
                }}
              >
                {t("rail.board_expand")}
              </button>
            </div>
            <BoardSection
              board={board}
              onExpand={() => {
                setDetailId(null);
                setOverlayOpen(true);
              }}
              onOpenItem={(id) => {
                setDetailId(id);
                setOverlayOpen(true);
              }}
            />
          </div>
        </div>
      )}

      {overlayOpen && board && (
        <BoardOverlay
          board={board}
          onClose={() => {
            setOverlayOpen(false);
            setDetailId(null);
            void refresh(token, space);
          }}
          loadItem={loadItem}
          loadAttachment={loadAttachment}
          initialItem={detailId}
          onTransition={(item, to, comment) => {
            void tokenBoardTransition(space.trim(), item, to, comment ?? "", token.trim()).then(
              () => refresh(token, space),
            );
          }}
          onComment={(item, body) =>
            tokenBoardComment(space.trim(), item, body, token.trim()).then(() =>
              refresh(token, space),
            )
          }
        />
      )}
    </div>
  );
}
