// The home page's one scene. Scrolling through `.film` scrubs it from 0 to 1:
//   0.00  six camera pictures adrift around the headline
//   0.45  they have come together into the grid they have in the window
//   0.53  the window itself has faded in around them
//   0.84  the window has changed to another layout
// Only transform, opacity and filter change, on eight elements. No library, nothing loaded from
// anywhere else.
(() => {
  const film = document.getElementById("film");
  const stage = document.getElementById("stage");
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;

  const clamp = (x) => Math.min(1, Math.max(0, x));
  const ease = (x) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2);
  const mix = (a, b, t) => a + (b - a) * t;

  // The transport at the foot of the page follows the whole page, in the time of the footage shown.
  const played = document.getElementById("played");
  const clock = document.getElementById("clock");
  const start = 14 * 3600 + 33 * 60, length = 114;
  function transport() {
    const room = document.documentElement.scrollHeight - innerHeight;
    const at = room > 0 ? clamp(scrollY / room) : 0;
    played.style.transform = `scaleX(${at})`;
    const t = Math.round(start + at * length);
    const two = (n) => String(n).padStart(2, "0");
    clock.textContent = `${two(Math.floor(t / 3600))}:${two(Math.floor(t / 60) % 60)}:${two(t % 60)}`;
  }

  if (still) {
    addEventListener("scroll", transport, { passive: true });
    transport();
    return;
  }

  // Where each camera's picture is in the screenshot (fractions of it), and where it drifts
  // before the scene begins: across (of the viewport's width), down (of its height), size, turn
  // in degrees, and how far away it is (blur, and how slowly it bobs).
  const X = [0.205, 0.4707, 0.7357], Y = [0.1186, 0.527], W = 0.258, H = 0.289;
  const adrift = [
    [-0.2, -0.27, 0.72, -7, 3], [0.03, -0.4, 0.5, 3, 6], [0.22, -0.25, 0.86, 6, 1],
    [-0.24, 0.27, 1.2, 5, 0], [-0.04, 0.4, 0.56, -3, 5], [0.23, 0.3, 1.1, -6, 2],
  ];
  const tiles = adrift.map((drift, i) => {
    const [x, y] = [X[i % 3], Y[Math.floor(i / 3)]];
    const tile = document.createElement("div");
    tile.className = "tile";
    tile.setAttribute("aria-hidden", "true");
    Object.assign(tile.style, {
      left: `${x * 100}%`, top: `${y * 100}%`, width: `${W * 100}%`, height: `${H * 100}%`,
      backgroundSize: `${100 / W}% ${100 / H}%`,
      backgroundPosition: `${(x / (1 - W)) * 100}% ${(y / (1 - H)) * 100}%`,
    });
    stage.appendChild(tile);
    return { tile, drift };
  });

  const lines = [...document.querySelectorAll(".line")];
  // When each line is on: from, to (the last one stays).
  const on = [[-1, 0.09], [0.2, 0.47], [0.53, 0.72], [0.8, 2]];
  const shotA = document.getElementById("shot-a");
  const shotB = document.getElementById("shot-b");
  document.documentElement.classList.add("live");

  function draw(now) {
    const room = film.offsetHeight - innerHeight;
    const p = room > 0 ? clamp(-film.getBoundingClientRect().top / room) : 1;
    const together = ease(clamp((p - 0.06) / 0.39));
    const apart = 1 - together;
    const phone = innerWidth < 700;

    tiles.forEach(({ tile, drift: [dx, dy, size, turn, far] }, i) => {
      const bob = Math.sin(now / (1400 + far * 260) + i * 1.7) * 10 * apart;
      const x = dx * innerWidth * (phone ? 1.5 : 1) * apart;
      const y = dy * innerHeight * apart + bob;
      tile.style.transform = `translate3d(${x}px, ${y}px, 0) scale(${mix(size, 1, together)}) rotate(${turn * apart}deg)`;
      tile.style.filter = apart > 0.01 ? `blur(${far * apart}px)` : "none";
      // Once the window is fully there the pictures are part of it.
      tile.style.opacity = 1 - clamp((p - 0.55) / 0.04);
    });

    shotA.style.opacity = clamp((p - 0.45) / 0.08);
    shotB.style.opacity = clamp((p - 0.76) / 0.08);
    stage.style.transform = `scale(${mix(0.9, 1, ease(clamp((p - 0.4) / 0.3)))})`;

    lines.forEach((line, k) => {
      const [from, to] = on[k];
      const shown = Math.min(clamp((p - from) / 0.05), clamp((to - p) / 0.05));
      line.style.opacity = shown;
      line.style.transform = `translate3d(0, ${(1 - shown) * (p < from ? 14 : -14)}px, 0)`;
    });

    transport();
    requestAnimationFrame(draw);
  }
  requestAnimationFrame(draw);
})();
