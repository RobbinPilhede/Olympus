//! Force-directed graph layout, ported from d3-force (ISC, Copyright 2010-2021 Mike Bostock).

const NONE: u32 = u32::MAX;
const THETA2: f32 = 0.81;
const MAX_DEPTH: u32 = 24;

/// The force settings of a simulation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Forces {
    pub center: f32,
    pub repel: f32,
    pub link_strength: f32,
    pub link_distance: f32,
    pub collide_radius: f32,
    pub collide_strength: f32,
    pub dmin: f32,
    pub velocity_decay: f32,
}

impl Default for Forces {
    fn default() -> Self {
        Self { center: 0.1, repel: -1000.0, link_strength: 1.0, link_distance: 250.0, collide_radius: 60.0, collide_strength: 0.5, dmin: 30.0, velocity_decay: 0.6 }
    }
}

#[derive(Debug, Clone, Copy)]
struct Cell {
    sx: f32,
    sy: f32,
    count: u32,
    head: u32,
    kids: [u32; 4],
}

impl Cell {
    const EMPTY: Cell = Cell { sx: 0.0, sy: 0.0, count: 0, head: NONE, kids: [NONE; 4] };

    fn add(&mut self, (x, y): (f32, f32)) {
        self.sx += x;
        self.sy += y;
        self.count += 1;
    }
}

/// A running layout over a fixed set of nodes.
#[derive(Debug, Clone)]
pub struct Sim {
    forces: Forces,
    pos: Vec<(f32, f32)>,
    vel: Vec<(f32, f32)>,
    pins: Vec<Option<(f32, f32)>>,
    links: Vec<(u32, u32, f32, f32)>,
    alpha: f32,
    target: f32,
    seed: u32,
    cells: Vec<Cell>,
    next: Vec<u32>,
    stack: Vec<(u32, f32)>,
    grid_start: Vec<u32>,
    grid_items: Vec<u32>,
}

impl Sim {
    const ALPHA_MIN: f32 = 0.001;

    /// Seeds `node_count` nodes on a phyllotaxis spiral; self-loops and out-of-range edges are dropped.
    pub fn new(node_count: usize, edges: &[(usize, usize)], forces: Forces) -> Self {
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        let pos = (0..node_count).map(|i| { let r = 10.0 * (0.5 + i as f32).sqrt(); let a = i as f32 * golden; (r * a.cos(), r * a.sin()) }).collect();
        let valid: Vec<(usize, usize)> = edges.iter().copied().filter(|&(s, t)| s != t && s < node_count && t < node_count).collect();
        let mut degree = vec![0u32; node_count];
        for &(s, t) in &valid {
            degree[s] += 1;
            degree[t] += 1;
        }
        let links = valid.iter().map(|&(s, t)| { let (ds, dt) = (degree[s] as f32, degree[t] as f32); (s as u32, t as u32, 1.0 / ds.min(dt), ds / (ds + dt)) }).collect();
        Self { forces, pos, vel: vec![(0.0, 0.0); node_count], pins: vec![None; node_count], links, alpha: 0.3, target: 0.0, seed: 1, cells: Vec::new(), next: vec![NONE; node_count], stack: Vec::new(), grid_start: Vec::new(), grid_items: Vec::new() }
    }

    /// Advances the layout one step; does nothing once settled.
    pub fn tick(&mut self) {
        if self.settled() {
            return;
        }
        self.alpha += (self.target - self.alpha) * (1.0 - 0.001f32.powf(1.0 / 300.0));
        self.center();
        self.repel();
        self.link();
        self.collide();
        self.integrate();
    }

    /// Current node positions, by node index.
    pub fn positions(&self) -> &[(f32, f32)] {
        &self.pos
    }

    /// True once the layout has cooled and nothing is keeping it warm.
    pub fn settled(&self) -> bool {
        self.alpha < Self::ALPHA_MIN && self.target < Self::ALPHA_MIN
    }

    /// Sets the current heat, waking a settled layout.
    pub fn reheat(&mut self, alpha: f32) {
        if alpha.is_finite() {
            self.alpha = alpha.max(0.0);
        }
    }

    /// Sets the heat the layout decays towards; above zero it never settles, as during a drag.
    pub fn set_target(&mut self, alpha: f32) {
        if alpha.is_finite() {
            self.target = alpha.max(0.0);
        }
    }

    /// Holds a node at a point, or frees it with `None`. Out-of-range or non-finite pins are ignored.
    pub fn pin(&mut self, index: usize, at: Option<(f32, f32)>) {
        if index >= self.pos.len() || at.is_some_and(|(x, y)| !x.is_finite() || !y.is_finite()) {
            return;
        }
        self.pins[index] = at;
        if let Some(p) = at {
            self.pos[index] = p;
            self.vel[index] = (0.0, 0.0);
        }
    }

    /// Replaces the force settings; the next tick uses them.
    pub fn set_forces(&mut self, forces: Forces) {
        self.forces = forces;
    }

    fn jiggle(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let j = (self.seed as f32 / 4_294_967_296.0 - 0.5) * 1e-3;
        if j == 0.0 { 1e-4 } else { j }
    }

    fn center(&mut self) {
        let k = self.forces.center * self.alpha;
        for (v, p) in self.vel.iter_mut().zip(&self.pos) {
            v.0 -= p.0 * k;
            v.1 -= p.1 * k;
        }
    }

    fn build_tree(&mut self) -> f32 {
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in &self.pos {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        let width = (x1 - x0).max(y1 - y0).max(1.0);
        self.cells.clear();
        self.cells.push(Cell::EMPTY);
        for i in 0..self.pos.len() {
            self.insert(i as u32, x0, y0, width);
        }
        width
    }

    fn insert(&mut self, i: u32, mut x0: f32, mut y0: f32, mut w: f32) {
        let p = self.pos[i as usize];
        let (mut c, mut depth) = (0usize, 0u32);
        loop {
            let cell = self.cells[c];
            if cell.head != NONE {
                let held = self.pos[cell.head as usize];
                if held == p || depth >= MAX_DEPTH {
                    self.next[i as usize] = cell.head;
                    self.cells[c].head = i;
                    self.cells[c].add(p);
                    return;
                }
                let q = quadrant(held, x0, y0, w);
                self.cells.push(cell);
                self.cells[c].head = NONE;
                self.cells[c].kids[q] = self.cells.len() as u32 - 1;
            } else if cell.count == 0 {
                self.next[i as usize] = NONE;
                self.cells[c].head = i;
                self.cells[c].add(p);
                return;
            }
            self.cells[c].add(p);
            let q = quadrant(p, x0, y0, w);
            w *= 0.5;
            x0 += if q & 1 == 1 { w } else { 0.0 };
            y0 += if q & 2 == 2 { w } else { 0.0 };
            depth += 1;
            if self.cells[c].kids[q] == NONE {
                self.cells.push(Cell::EMPTY);
                self.cells[c].kids[q] = self.cells.len() as u32 - 1;
            }
            c = self.cells[c].kids[q] as usize;
        }
    }

    fn repel(&mut self) {
        let strength = self.forces.repel;
        if strength == 0.0 || self.pos.is_empty() {
            return;
        }
        let width = self.build_tree();
        let dmin2 = self.forces.dmin * self.forces.dmin;
        let mut stack = std::mem::take(&mut self.stack);
        for i in 0..self.pos.len() {
            let p = self.pos[i];
            let mut v = self.vel[i];
            stack.clear();
            stack.push((0, width));
            while let Some((c, w)) = stack.pop() {
                let cell = self.cells[c as usize];
                if cell.count == 0 {
                    continue;
                }
                let count = cell.count as f32;
                let (mut dx, mut dy) = (cell.sx / count - p.0, cell.sy / count - p.1);
                let mut l = dx * dx + dy * dy;
                if w * w / THETA2 < l {
                    (dx, dy, l) = self.separate(dx, dy, l);
                    if l < dmin2 {
                        l = (dmin2 * l).sqrt();
                    }
                    let k = count * strength * self.alpha / l;
                    v.0 += dx * k;
                    v.1 += dy * k;
                    continue;
                }
                if cell.head == NONE {
                    stack.extend(cell.kids.iter().filter(|&&k| k != NONE).map(|&k| (k, w * 0.5)));
                    continue;
                }
                let mut point = cell.head;
                while point != NONE {
                    if point as usize != i {
                        let q = self.pos[point as usize];
                        let (dx, dy) = (q.0 - p.0, q.1 - p.1);
                        let (dx, dy, mut l) = self.separate(dx, dy, dx * dx + dy * dy);
                        if l < dmin2 {
                            l = (dmin2 * l).sqrt();
                        }
                        let k = strength * self.alpha / l;
                        v.0 += dx * k;
                        v.1 += dy * k;
                    }
                    point = self.next[point as usize];
                }
            }
            self.vel[i] = v;
        }
        self.stack = stack;
    }

    fn separate(&mut self, mut dx: f32, mut dy: f32, mut l: f32) -> (f32, f32, f32) {
        if dx == 0.0 {
            dx = self.jiggle();
            l += dx * dx;
        }
        if dy == 0.0 {
            dy = self.jiggle();
            l += dy * dy;
        }
        (dx, dy, l)
    }

    fn link(&mut self) {
        let (distance, strength, alpha) = (self.forces.link_distance, self.forces.link_strength, self.alpha);
        for index in 0..self.links.len() {
            let (s, t, weight, bias) = self.links[index];
            let (s, t) = (s as usize, t as usize);
            let dx = self.pos[t].0 + self.vel[t].0 - self.pos[s].0 - self.vel[s].0;
            let dy = self.pos[t].1 + self.vel[t].1 - self.pos[s].1 - self.vel[s].1;
            let (dx, dy, l2) = self.separate(dx, dy, dx * dx + dy * dy);
            let l = l2.sqrt();
            let f = (l - distance) / l * alpha * strength * weight;
            self.vel[t].0 -= dx * f * bias;
            self.vel[t].1 -= dy * f * bias;
            self.vel[s].0 += dx * f * (1.0 - bias);
            self.vel[s].1 += dy * f * (1.0 - bias);
        }
    }

    fn collide(&mut self) {
        let radius = self.forces.collide_radius;
        let reach = radius + radius;
        let n = self.pos.len();
        if reach <= 0.0 || self.forces.collide_strength == 0.0 || n < 2 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (p, v) in self.pos.iter().zip(&self.vel) {
            x0 = x0.min(p.0 + v.0);
            y0 = y0.min(p.1 + v.1);
            x1 = x1.max(p.0 + v.0);
            y1 = y1.max(p.1 + v.1);
        }
        // Cells may only grow past `reach`, so a far-flung node cannot inflate the grid's memory.
        let size = reach.max((x1 - x0).max(y1 - y0) / (2.0 * (n as f32).sqrt()).max(1.0));
        let (cols, rows) = (((x1 - x0) / size) as usize + 1, ((y1 - y0) / size) as usize + 1);
        let at = |x: f32, y: f32| (((x - x0) / size) as usize).min(cols - 1) + ((((y - y0) / size) as usize).min(rows - 1)) * cols;
        self.grid_start.clear();
        self.grid_start.resize(cols * rows + 1, 0);
        for (p, v) in self.pos.iter().zip(&self.vel) {
            self.grid_start[at(p.0 + v.0, p.1 + v.1) + 1] += 1;
        }
        for c in 1..self.grid_start.len() {
            self.grid_start[c] += self.grid_start[c - 1];
        }
        self.grid_items.clear();
        self.grid_items.resize(n, 0);
        let mut fill = self.grid_start.clone();
        for i in 0..n {
            let cell = at(self.pos[i].0 + self.vel[i].0, self.pos[i].1 + self.vel[i].1);
            self.grid_items[fill[cell] as usize] = i as u32;
            fill[cell] += 1;
        }
        let strength = self.forces.collide_strength;
        for i in 0..n {
            let home = at(self.pos[i].0 + self.vel[i].0, self.pos[i].1 + self.vel[i].1);
            let (col, row) = (home % cols, home / cols);
            for r in row.saturating_sub(1)..=(row + 1).min(rows - 1) {
                for c in col.saturating_sub(1)..=(col + 1).min(cols - 1) {
                    let cell = r * cols + c;
                    for slot in self.grid_start[cell]..self.grid_start[cell + 1] {
                        let j = self.grid_items[slot as usize] as usize;
                        if j > i {
                            self.push_apart(i, j, reach, strength);
                        }
                    }
                }
            }
        }
    }

    fn push_apart(&mut self, i: usize, j: usize, reach: f32, strength: f32) {
        let dx = self.pos[i].0 - self.pos[j].0 - self.vel[j].0;
        let dy = self.pos[i].1 - self.pos[j].1 - self.vel[j].1;
        let l2 = dx * dx + dy * dy;
        if l2 >= reach * reach {
            return;
        }
        let (dx, dy, l2) = self.separate(dx, dy, l2);
        let l = l2.sqrt();
        // All radii are equal, so the mass share of the d3 formula is always one half.
        let f = (reach - l) / l * strength * 0.5;
        self.vel[i].0 += dx * f;
        self.vel[i].1 += dy * f;
        self.vel[j].0 -= dx * f;
        self.vel[j].1 -= dy * f;
    }

    fn integrate(&mut self) {
        let keep = self.forces.velocity_decay;
        for i in 0..self.pos.len() {
            if let Some(p) = self.pins[i] {
                self.pos[i] = p;
                self.vel[i] = (0.0, 0.0);
                continue;
            }
            let v = (self.vel[i].0 * keep, self.vel[i].1 * keep);
            let p = (self.pos[i].0 + v.0, self.pos[i].1 + v.1);
            if p.0.is_finite() && p.1.is_finite() {
                self.pos[i] = p;
                self.vel[i] = v;
            } else {
                self.vel[i] = (0.0, 0.0);
            }
        }
    }
}

fn quadrant((x, y): (f32, f32), x0: f32, y0: f32, w: f32) -> usize {
    let half = w * 0.5;
    usize::from(x >= x0 + half) | usize::from(y >= y0 + half) << 1
}

/// On-screen radius of a node with this many links.
pub fn node_radius(degree: usize, size_multiplier: f32) -> f32 {
    size_multiplier * (4.0 + (degree as f32).sqrt())
}

/// Opacity of node labels at a zoom level; `fade` shifts the zoom where they appear.
pub fn label_alpha(zoom: f32, fade: f32) -> f32 {
    if zoom <= 0.0 {
        return 0.0;
    }
    ((zoom.log2() + fade + 1.0) / 0.5).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(sim: &mut Sim) -> usize {
        let mut ticks = 0;
        while !sim.settled() && ticks < 2000 {
            sim.tick();
            ticks += 1;
        }
        ticks
    }

    fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
        ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
    }

    fn finite(sim: &Sim) -> bool {
        sim.positions().iter().all(|p| p.0.is_finite() && p.1.is_finite())
    }

    #[test]
    fn seeds_follow_the_spiral() {
        let sim = Sim::new(3, &[], Forces::default());
        let radius = |p: (f32, f32)| dist(p, (0.0, 0.0));
        assert!((radius(sim.positions()[0]) - 10.0 * 0.5f32.sqrt()).abs() < 1e-3);
        assert!((radius(sim.positions()[2]) - 10.0 * 2.5f32.sqrt()).abs() < 1e-3);
    }

    #[test]
    fn cools_down_and_stops_moving() {
        let mut sim = Sim::new(50, &[(0, 1), (1, 2)], Forces::default());
        let ticks = run(&mut sim);
        assert!((200..300).contains(&ticks), "took {ticks} ticks");
        let before = sim.positions().to_vec();
        sim.tick();
        assert_eq!(before, sim.positions());
    }

    #[test]
    fn linked_nodes_end_nearer_than_unlinked_ones() {
        let edges: Vec<(usize, usize)> = (0..60).step_by(2).map(|i| (i, i + 1)).collect();
        let mut sim = Sim::new(60, &edges, Forces::default());
        run(&mut sim);
        let p = sim.positions();
        let linked: f32 = edges.iter().map(|&(a, b)| dist(p[a], p[b])).sum::<f32>() / edges.len() as f32;
        let (mut far, mut pairs) = (0.0, 0.0);
        for a in 0..60 {
            for b in a + 1..60 {
                if !edges.contains(&(a, b)) {
                    far += dist(p[a], p[b]);
                    pairs += 1.0;
                }
            }
        }
        assert!(linked < far / pairs, "linked {linked}, unlinked {}", far / pairs);
    }

    #[test]
    fn link_settles_near_its_rest_length() {
        let mut sim = Sim::new(2, &[(0, 1)], Forces { repel: 0.0, center: 0.0, collide_strength: 0.0, ..Forces::default() });
        for _ in 0..3 {
            sim.reheat(0.3);
            run(&mut sim);
        }
        let d = dist(sim.positions()[0], sim.positions()[1]);
        assert!((d - 250.0).abs() < 25.0, "distance {d}");
    }

    #[test]
    fn coincident_nodes_separate_without_nan() {
        let mut sim = Sim::new(40, &[(0, 1), (2, 3)], Forces::default());
        for i in 0..40 {
            sim.pin(i, Some((7.0, 7.0)));
        }
        for i in 0..40 {
            sim.pin(i, None);
        }
        for _ in 0..200 {
            sim.tick();
            assert!(finite(&sim));
        }
        assert!(dist(sim.positions()[0], sim.positions()[1]) > 1.0);
    }

    #[test]
    fn repulsion_alone_handles_two_coincident_nodes() {
        let mut sim = Sim::new(2, &[], Forces { collide_strength: 0.0, center: 0.0, ..Forces::default() });
        sim.pin(0, Some((0.0, 0.0)));
        sim.pin(1, Some((0.0, 0.0)));
        sim.pin(0, None);
        sim.pin(1, None);
        sim.tick();
        assert!(finite(&sim));
        assert!(dist(sim.positions()[0], sim.positions()[1]) > 0.0);
    }

    #[test]
    fn survives_empty_and_single_node_and_bad_edges() {
        let mut none = Sim::new(0, &[(0, 1)], Forces::default());
        none.tick();
        assert!(none.positions().is_empty());
        let mut one = Sim::new(1, &[(0, 0), (0, 9)], Forces::default());
        run(&mut one);
        assert!(finite(&one));
    }

    #[test]
    fn extreme_positions_stay_finite() {
        let mut sim = Sim::new(30, &[(0, 1)], Forces::default());
        sim.pin(0, Some((1e30, -1e30)));
        sim.pin(0, None);
        for _ in 0..100 {
            sim.tick();
            assert!(finite(&sim));
        }
    }

    #[test]
    fn pinned_node_holds_while_others_move() {
        let mut sim = Sim::new(10, &[(0, 1)], Forces::default());
        sim.pin(0, Some((500.0, -500.0)));
        let start = sim.positions()[1];
        for _ in 0..30 {
            sim.tick();
        }
        assert_eq!(sim.positions()[0], (500.0, -500.0));
        assert_ne!(sim.positions()[1], start);
    }

    #[test]
    fn invalid_pins_are_ignored() {
        let mut sim = Sim::new(1, &[], Forces::default());
        let start = sim.positions()[0];
        sim.pin(0, Some((f32::NAN, 0.0)));
        sim.pin(5, Some((1.0, 1.0)));
        assert_eq!(sim.positions()[0], start);
    }

    #[test]
    fn reheat_wakes_a_settled_layout() {
        let mut sim = Sim::new(5, &[(0, 1)], Forces::default());
        run(&mut sim);
        assert!(sim.settled());
        sim.reheat(0.3);
        assert!(!sim.settled());
    }

    #[test]
    fn a_held_target_keeps_it_awake() {
        let mut sim = Sim::new(5, &[], Forces::default());
        sim.set_target(0.3);
        assert_eq!(run(&mut sim), 2000);
    }

    #[test]
    fn same_inputs_give_the_same_layout() {
        let build = || { let mut s = Sim::new(100, &[(0, 1), (5, 9)], Forces::default()); run(&mut s); s };
        assert_eq!(build().positions(), build().positions());
    }

    #[test]
    fn set_forces_changes_the_outcome() {
        let mut wide = Sim::new(2, &[(0, 1)], Forces::default());
        let mut narrow = Sim::new(2, &[(0, 1)], Forces::default());
        narrow.set_forces(Forces { link_distance: 10.0, collide_strength: 0.0, repel: 0.0, ..Forces::default() });
        run(&mut wide);
        run(&mut narrow);
        assert!(dist(narrow.positions()[0], narrow.positions()[1]) < dist(wide.positions()[0], wide.positions()[1]));
    }

    #[test]
    fn radius_grows_with_degree() {
        assert_eq!(node_radius(0, 1.0), 4.0);
        assert_eq!(node_radius(16, 2.0), 16.0);
    }

    #[test]
    fn labels_fade_in_with_zoom() {
        assert_eq!(label_alpha(1.0, 0.0), 1.0);
        assert_eq!(label_alpha(0.25, 0.0), 0.0);
        assert_eq!(label_alpha(0.5, 0.25), 0.5);
        assert_eq!(label_alpha(0.0, 0.0), 0.0);
    }

    #[test]
    #[ignore = "timing, run with --release -- --ignored --nocapture"]
    fn five_thousand_nodes_tick_quickly() {
        let edges: Vec<(usize, usize)> = (1..5000).map(|i| (i, i / 7)).chain((0..2500).map(|i| (i, (i * 13 + 5) % 5000))).collect();
        let mut sim = Sim::new(5000, &edges, Forces::default());
        let start = std::time::Instant::now();
        let ticks = run(&mut sim);
        let per_tick = start.elapsed().as_secs_f32() * 1000.0 / ticks as f32;
        println!("5000 nodes, {} edges: {ticks} ticks, {per_tick:.2} ms per tick", edges.len());
        assert!(finite(&sim));
        assert!(per_tick < 50.0, "{per_tick} ms per tick");
    }
}
