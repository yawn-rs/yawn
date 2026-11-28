// Generic components: the props a caller passes decide the type parameters.
use std::fmt::Display;

/// `T` comes from `value`
component Labeled<T: Display + Clone + 'static>(value: T, label: String = "value".to_string()) {
    view { <span class="labeled">"{label}: {value}"</span> }
}

/// `->` inside the generics, and a default (`sep`) whose type says nothing about `T` and `F`
component Joined<T, F: Fn(T) -> String>(items: Vec<T>, show: F, sep: String = ", ".to_string()) {
    let text = items.into_iter().map(show).collect::<Vec<_>>().join(&sep);
    view { <p class="joined">"{text}"</p> }
}

/// The bounds in a `where` clause
component Repeated<T>(value: T, times: usize = 2)
where
    T: Display, // what the view prints
{
    let text = value.to_string().repeat(times);
    view { <b class="repeated">"{text}"</b> }
}

component Uses() {
    view {
        <div>
            <Labeled value={3} />
            <Labeled value={"x"} label="name" />
            <Joined items={vec![1, 2, 3]} show={|n: i32| format!("#{n}")} />
            <Repeated value={'a'} />
        </div>
    }
}
