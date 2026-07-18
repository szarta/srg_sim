// The outstanding decision: renders each legal option as a button. Clicking submits
// its index back through the WASM Session. Option shapes come straight from the
// engine's `legal` list ({kind:"play", card, number, atk_type, order} | {kind:"pass"} | …).
export default function DecisionPanel({ request, onSubmit }) {
  const { viewer, point, legal, observable_state } = request
  const hand = observable_state.players[viewer]?.hand ?? []

  const label = (o) => {
    if (o.kind === 'pass') return 'Pass'
    if (o.kind === 'play') {
      const card = hand.find((c) => c.db_uuid === o.card)
      return `Play ${card ? card.name : `#${o.number}`} · ${o.atk_type} ${o.order}`
    }
    // Unknown/other decision kinds: show the raw option so nothing is hidden.
    return o.kind ? o.kind : JSON.stringify(o)
  }

  return (
    <div className="rounded-lg border border-neutral-700 bg-neutral-900 p-3">
      <div className="mb-2 text-sm">
        <span className="font-semibold text-amber-300">{viewer}</span> to decide
        <span className="ml-2 text-xs text-neutral-400">{point}</span>
      </div>
      <div className="flex flex-wrap gap-2">
        {legal.map((o, i) => (
          <button
            key={i}
            onClick={() => onSubmit(i)}
            className="rounded-md border border-neutral-600 bg-neutral-800 px-3 py-1.5 text-sm hover:border-amber-400 hover:bg-neutral-700"
          >
            {label(o)}
          </button>
        ))}
      </div>
    </div>
  )
}
