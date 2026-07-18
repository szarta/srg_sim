import CardChip from './CardChip.jsx'
import DiscardStack from './DiscardStack.jsx'

// One player's half of the mat, rendered from an observable player-view. The
// viewer's own hand is face-up; an opponent's is a hidden count (unless a Peek
// revealed it, in which case the engine already put the cards in `hand`).
export default function Board({ label, view, isSelf, isActive }) {
  const comp = view.competitor
  const hand = view.hand // array when visible, else undefined
  return (
    <section
      className={[
        'rounded-lg border p-3',
        isActive ? 'border-amber-400/70 bg-amber-400/5' : 'border-neutral-800 bg-neutral-900/40',
      ].join(' ')}
    >
      <header className="mb-2 flex items-center justify-between">
        <div>
          <span className="text-xs uppercase tracking-wide text-neutral-500">{label}</span>{' '}
          <span className="font-semibold">{comp.name}</span>
          <span className="ml-2 text-xs text-neutral-400">{comp.division}</span>
          {view.gimmick_blanked && (
            <span className="ml-2 rounded bg-rose-500/20 px-1.5 py-0.5 text-[10px] text-rose-300">
              gimmick blanked
            </span>
          )}
        </div>
        <span className="text-xs text-neutral-400">deck {view.deck_size}</span>
      </header>

      <Row label="In play">
        {view.in_play.length ? (
          view.in_play.map((c, i) => <CardChip key={`${c.db_uuid}-${i}`} card={c} />)
        ) : (
          <Empty />
        )}
      </Row>

      <Row label={isSelf ? 'Hand' : 'Hand (hidden)'}>
        {hand ? (
          hand.length ? (
            hand.map((c, i) => <CardChip key={`${c.db_uuid}-${i}`} card={c} />)
          ) : (
            <Empty />
          )
        ) : (
          <span className="text-sm text-neutral-500">{view.hand_size} cards</span>
        )}
      </Row>

      <div className="mt-2">
        <DiscardStack cards={view.discard} />
      </div>
    </section>
  )
}

function Row({ label, children }) {
  return (
    <div className="mb-2">
      <div className="mb-1 text-[10px] uppercase tracking-wide text-neutral-500">{label}</div>
      <div className="flex flex-wrap gap-1.5">{children}</div>
    </div>
  )
}

function Empty() {
  return <span className="text-sm text-neutral-600">—</span>
}
