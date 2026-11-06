//! Native 45017 partition membership and candidate traversal.
//!
//! Cell and ball sets use pointer keys in the original. Stable 32-bit tokens
//! preserve that algorithm without depending on this process's 64-bit heap.
//! They do not reconstruct the unavailable original server allocation history.
use super::{Vec3, candidates::{self, PointerSet}};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const WIDTHS: [f64; 8] = [245_760.0, 61_440.0, 15_360.0, 3_840.0, 960.0, 240.0, 60.0, 15.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key { pub level: u32, pub at: [i64; 3] }
impl Key {
    fn parent(self) -> Option<Self> {
        self.level.checked_sub(1).map(|level| Self { level, at: self.at.map(|x| x >> 2) })
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Ball { pub id: i64, pub pos: Vec3, pub vel: Vec3, pub radius: f64 }
impl Ball {
    fn valid(self)->bool {self.pos.is_finite() && self.vel.is_finite() && self.radius.is_finite() && self.radius>=0.0}
}

#[derive(Debug, Clone)]
struct Entry {
    token: u32,
    primary: Key,
    neighbors: [i16; 3],
    cells: PointerSet<Key>,
    last: Ball,
}

#[derive(Debug, Clone)]
struct Cell { token: u32, balls: PointerSet<i64>, children: PointerSet<Key> }

#[derive(Debug, Clone)]
pub(super) struct Index {
    balls: BTreeMap<i64, Entry>,
    cells: BTreeMap<Key, Cell>,
    next_token: u32,
    pool: Vec<u32>,
    recycled: Vec<u32>,
    issued: BTreeSet<u32>,
}
impl Default for Index {
    fn default() -> Self {
        Self { balls: BTreeMap::new(), cells: BTreeMap::new(), next_token: 0x2000_1000,
            pool: Vec::new(), recycled: Vec::new(), issued: Default::default(), }
    }
}

impl Index {
    fn token(&mut self) -> u32 {
        if let Some(token)=self.recycled.pop() {return token;}
        loop {
            let token=self.next_token;
            self.next_token=self.next_token.wrapping_add(16);
            if token!=0 && self.issued.insert(token) {return token;}
        }
    }
    fn create(&mut self, key: Key) -> u32 {
        if let Some(cell) = self.cells.get(&key) { return cell.token; }
        let supplied: Option<u32> = None;
        let token = supplied.unwrap_or_else(|| self.pool.pop().unwrap_or_else(|| self.token()));
        self.cells.insert(key, Cell { token, balls: Default::default(), children: Default::default() });
        if let Some(parent) = key.parent() {
            self.create(parent);
            self.cells.get_mut(&parent).unwrap().children.insert(token,key);
        }
        token
    }
    fn discard_empty(&mut self, key: Key) {
        let Some(cell) = self.cells.get(&key) else { return };
        if cell.balls.len() != 0 || cell.children.len() != 0 { return; }
        let token = self.cells.remove(&key).unwrap().token;
        // Native destroys parent links before returning this cell to its
        // bounded 256-cell LIFO reuse pool (0x220f0 / 0x22210).
        if let Some(parent) = key.parent() {
            self.cells.get_mut(&parent).unwrap().children.remove(token);
            self.discard_empty(parent);
        }
        if self.pool.len() < 256 { self.pool.push(token); }else{self.recycled.push(token);}
    }
    pub fn remove(&mut self, id: i64) {
        if let Some(entry)=self.detach(id) {self.recycled.push(entry.token);}
    }
    fn detach(&mut self,id:i64)->Option<Entry> {
        let entry=self.balls.remove(&id)?;
        for key in entry.cells.ordered() {
            self.cells.get_mut(&key).unwrap().balls.remove(entry.token);
            self.discard_empty(key);
        }
        Some(entry)
    }
    /// New balls register immediately. Existing balls repartition only after
    /// a >=0.1 m² step displacement, as Evolve 0x1519f does. Call both before
    /// and after each tick so external arrivals and removals are reconciled.
    pub fn sync(&mut self, balls: impl IntoIterator<Item=Ball>) {
        let balls: Vec<_> = balls.into_iter().filter(|b|b.valid()).collect();
        let present: BTreeSet<_> = balls.iter().map(|b|b.id).collect();
        for id in self.balls.keys().copied().filter(|id|!present.contains(id)).collect::<Vec<_>>() { self.remove(id); }
        for ball in balls {
            let changed = self.balls.get(&ball.id).is_none_or(|old|
                (ball.pos-old.last.pos).len_sq() >= 0.1 || ball.radius != old.last.radius);
            if changed { self.update(ball); }
            if let Some(entry)=self.balls.get_mut(&ball.id) { entry.last=ball; }
        }
    }
    pub fn update(&mut self, ball: Ball) {
        if !ball.valid() {self.remove(ball.id);return;}
        let (primary, neighbors) = membership(ball);
        if let Some(entry) = self.balls.get_mut(&ball.id) {
            if entry.primary == primary && entry.neighbors == neighbors { entry.last=ball; return; }
        }
        // DoPartition creates the prospective primary before removing the old
        // registration. Empty-cell removal may then destroy/recreate it.
        self.create(primary);
        let existing = self.balls.get(&ball.id).map(|e|e.token);
        self.detach(ball.id);
        let supplied:Option<u32>=None;
        let token=existing.or(supplied).unwrap_or_else(||self.token());
        let mut cells=PointerSet::default();
        // Native insertion sequence: center, x, y, z, xy, xz, yz, xyz.
        for mask in [0,1,2,4,3,5,6,7] {
            if (0..3).any(|axis| mask & (1<<axis)!=0 && neighbors[axis]==0) { continue; }
            let limit=(1i64 << (28+2*primary.level))-1;
            let key=Key {level:primary.level,at:std::array::from_fn(|axis|
                (primary.at[axis]+if mask & (1<<axis)!=0 {i64::from(neighbors[axis])}else{0}).clamp(0,limit))};
            let cell_token=self.create(key);
            self.cells.get_mut(&key).unwrap().balls.insert(token,ball.id);
            cells.insert(cell_token,key);
        }
        self.balls.insert(ball.id,Entry {token,primary,neighbors,cells,last:ball});
    }
    fn descendants(&self, key: Key, out: &mut Vec<Key>) {
        for child in self.cells[&key].children.ordered() {
            self.descendants(child,out);
            if self.cells[&child].balls.len()!=0 { out.push(child); }
        }
    }
    /// Run the original ancestor/descendant query including repeated ball
    /// insertions. Filtering precedes ID-map insertion in native 0x2980a.
    pub fn candidates(&self, id: i64, mut include: impl FnMut(i64)->bool) -> Vec<i64> {
        let Some(entry)=self.balls.get(&id) else { return Vec::new() };
        let mut visited=BTreeSet::new();let mut cells=Vec::new();
        for key in entry.cells.ordered() {
            if visited.contains(&key) { continue; }
            let mut parent=key.parent();
            while let Some(key)=parent {
                if visited.contains(&key) { break; }
                if self.cells[&key].balls.len()!=0 { cells.push(key);visited.insert(key); }
                parent=key.parent();
            }
            self.descendants(key,&mut cells);
            if self.cells[&key].balls.len()>1 { cells.push(key);visited.insert(key); }
        }
        candidates::order(cells.into_iter().flat_map(|key|self.cells[&key].balls.ordered())
            .filter(|other|*other!=id && include(*other)))
    }
}

pub(super) fn membership(ball: Ball) -> (Key,[i16;3]) {
    let reach=3.0*(ball.radius+ball.vel.len());
    let level=WIDTHS.iter().rposition(|width|*width>=reach).unwrap_or(0) as u32;
    let width=WIDTHS[level as usize];let grid=1i64<<(28+2*level);
    let pos=[ball.pos.x,ball.pos.y,ball.pos.z];let vel=[ball.vel.x,ball.vel.y,ball.vel.z];
    let at=pos.map(|p|coordinate(p,width,grid));
    let neighbors=std::array::from_fn(|axis| {
        let lower=(at[axis]-grid/2) as f64*width;
        if lower+ball.radius-pos[axis]>0.0 && vel[axis]<=0.0 { -1 }
        else if lower+width-ball.radius-pos[axis]<0.0 && vel[axis]>=0.0 { 1 }
        else { 0 }
    });
    (Key {level,at},neighbors)
}

/// Native 0x28fb7..0x2902f computes a reciprocal, translates, then multiplies.
/// ExeFile/blue startup selects a 53-bit x87 significand. The large translation
/// therefore rounds before multiplication, including points very near a cell
/// boundary. Retaining 64 bits here models a different control-word environment.
fn coordinate(position:f64,width:f64,grid:i64)->i64 {
    let offset=width*(grid/2) as f64;
    let sum=position+offset;
    if sum<=0.0 {return 0;}
    if sum>=2.0*offset {return grid-1;}
    ((sum*(1.0/width)) as i64).clamp(0,grid-1)
}
