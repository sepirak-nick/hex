import { useState } from "react";
import { createRoot } from "react-dom/client";
import { Agentation } from "agentation";
import "./styles.css";

function PricingCard({ name, price }) {
  const [chosen, setChosen] = useState(false);
  return (
    <section className="card" data-testid={`card-${name.toLowerCase()}`}>
      <h2 className="card-title">{name}</h2>
      <p className="card-price">{price}</p>
      <button className="choose" onClick={() => setChosen(!chosen)}>
        {chosen ? "Chosen" : "Choose"}
      </button>
    </section>
  );
}

function App() {
  return (
    <main style={{ fontFamily: "system-ui", padding: 32, maxWidth: 640 }}>
      <h1 className="hero-title">Talk and Click Demo</h1>
      <p>Start a Hex dictation, talk, and click things. Clicks only point while it records.</p>
      <PricingCard name="Trial" price="Free" />
      <PricingCard name="Organization" price="$500" />
      <Agentation endpoint="http://localhost:4747" />
    </main>
  );
}

createRoot(document.getElementById("root")).render(<App />);
