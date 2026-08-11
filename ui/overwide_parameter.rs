// A panicking arm for a variant that no existing call site passes: the
// parameter type is wider than the function's real domain.

enum Shape {
    Circle(u32),
    Square(u32),
    Line,
}

// Flagged: both call sites pass Circle or Square; the Line arm's panic
// cannot fire today and should be a compile error tomorrow.
fn area(s: Shape) -> u32 {
    match s {
        Shape::Circle(r) => 3 * r * r,
        Shape::Square(w) => w * w,
        Shape::Line => unreachable!("lines have no area"),
    }
}

// Fine: a call site passes Line, so the panic arm is genuinely reachable.
fn perimeter(s: Shape) -> u32 {
    match s {
        Shape::Circle(r) => 6 * r,
        Shape::Square(w) => 4 * w,
        Shape::Line => panic!("lines have no perimeter"),
    }
}

// Fine: the argument is not a constructor literal, so the call-site set is
// unknowable and the lint stays silent.
fn diameter(s: Shape) -> u32 {
    match s {
        Shape::Circle(r) => 2 * r,
        _ => unreachable!("only circles have diameters"),
    }
}

fn pick(n: u32) -> Shape {
    if n > 2 { Shape::Circle(n) } else { Shape::Square(n) }
}

fn main() {
    let _ = area(Shape::Circle(2));
    let _ = area(Shape::Square(3));
    let _ = perimeter(Shape::Circle(2));
    let _ = perimeter(Shape::Line);
    let _ = diameter(pick(4));
}
